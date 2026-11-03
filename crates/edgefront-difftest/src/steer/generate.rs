//! Generators for steer configuration documents and the requests aimed at
//! them.
//!
//! Configurations are written as JSON text rather than built from a value
//! tree, so the generator can produce what a person or a broken tool might
//! write: keys in the wrong case or folded through `ſ` and the Kelvin sign,
//! duplicate keys, unknown fields, `null`s, raw invalid UTF-8, truncated and
//! trailing data, and values at and just past every limit. Each document has
//! a fault rate; at zero it should be accepted, and above zero each field may
//! take a bad value, so about half of all documents are rejected and every
//! rule gets exercised from both sides.
//!
//! Requests aim at a document's routes: the hosts and prefixes it was
//! written with, decorated with case changes, ports, brackets, trailing dots,
//! dot segments, doubled slashes and percent escapes, alongside hosts and
//! targets that match nothing and targets that do not parse.

use crate::rng::Rng;

/// A generated configuration document, with the raw host patterns and path
/// prefixes its routes were written with, for the request generator.
#[derive(Debug, Clone)]
pub struct GenConfig {
    pub doc: Vec<u8>,
    pub hosts: Vec<String>,
    pub prefixes: Vec<String>,
}

/// One request: a Host header value and a request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenRequest {
    pub host: String,
    pub target: Vec<u8>,
}

const LABELS: &[&str] = &[
    "a", "b", "api", "www", "example", "com", "net", "x-y", "cdn", "shop", "eu", "1", "Edge",
];
const SEGMENTS: &[&str] = &[
    "api",
    "v1",
    "v2",
    "static",
    "a",
    "b",
    "img",
    "Index.html",
    "x.y",
    "~u",
    "-",
    "é",
    "a+b",
    "k=v",
    "@",
    "a;b",
    ":x",
];
const CLUSTER_NAMES: &[&str] = &["web", "api", "static", "a", "b.c", "x_y-z", "WEB", "0"];
const ADDRESSES: &[&str] = &[
    "10.0.0.1:80",
    "h:1",
    "[::1]:8080",
    "a-b.c:443",
    "api.internal:9000",
    "192.168.0.10:65535",
    "[fe80::1%eth0]:80",
    "UPPER.example:8443",
];

/// The document size limit steer enforced (`config.MaxFileBytes`).
const MAX_FILE_BYTES: usize = 1 << 20;

/// A JSON string literal for `s`.
fn jstr(s: &str) -> Vec<u8> {
    serde_json::to_vec(s).expect("strings always serialise")
}

/// An object under construction: its pairs, rendered with key faults.
#[derive(Default)]
struct Obj(Vec<(String, Vec<u8>)>);

impl Obj {
    fn put(&mut self, key: &str, value: Vec<u8>) {
        self.0.push((key.to_owned(), value));
    }

    fn render(self, g: &mut Gen) -> Vec<u8> {
        let mut pairs = self.0;
        if g.fault() && !pairs.is_empty() {
            match g.rng.below(5) {
                // encoding/json matches keys case-insensitively, with the
                // two non-ASCII folds of `s` and `k`.
                0 => {
                    let at = g.rng.below(pairs.len());
                    pairs[at].0 = fold_key(&mut g.rng, &pairs[at].0);
                }
                // A repeated key decodes over what the first one left.
                1 => {
                    let at = g.rng.below(pairs.len());
                    let dup = pairs[at].clone();
                    pairs.push(dup);
                }
                2 => {
                    let at = g.rng.below(pairs.len());
                    let key = pairs[at].0.clone();
                    pairs.push((key, b"null".to_vec()));
                }
                3 => pairs.push(("unknown_field".into(), b"1".to_vec())),
                _ => {
                    let at = g.rng.below(pairs.len());
                    pairs.remove(at);
                }
            }
        }
        let mut out = b"{".to_vec();
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            if g.rng.chance(0.05) {
                out.extend_from_slice(b"\n  ");
            }
            out.extend(jstr(k));
            out.push(b':');
            out.extend_from_slice(v);
        }
        out.push(b'}');
        out
    }
}

fn fold_key(rng: &mut Rng, key: &str) -> String {
    match rng.below(3) {
        0 => key.to_uppercase(),
        1 => key.replace('s', "ſ").replace('k', "\u{212a}"),
        _ => {
            let mut c = key.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
    }
}

fn array(items: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"[".to_vec();
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend(it);
    }
    out.push(b']');
    out
}

/// State for generating one document.
struct Gen {
    rng: Rng,
    /// The chance that any one field takes a bad value.
    fault_rate: f64,
}

impl Gen {
    fn fault(&mut self) -> bool {
        self.fault_rate > 0.0 && self.rng.chance(self.fault_rate)
    }

    /// A good value usually, a bad one on a fault.
    fn choose(&mut self, good: &[&str], bad: &[&str]) -> Vec<u8> {
        if self.fault() {
            match self.rng.below(8) {
                0 => b"5".to_vec(), // a number where a string belongs
                1 => b"null".to_vec(),
                _ => jstr(self.rng.pick(bad)),
            }
        } else {
            jstr(self.rng.pick(good))
        }
    }

    /// A good integer usually, a bad one on a fault. Bad values include
    /// floats and strings, which the decoder refuses.
    fn int(&mut self, good: &[&str], bad: &[&str]) -> Vec<u8> {
        if self.fault() {
            match self.rng.below(6) {
                0 => b"1.0".to_vec(),
                1 => b"\"2\"".to_vec(),
                2 => b"1e400".to_vec(),
                _ => self.rng.pick(bad).as_bytes().to_vec(),
            }
        } else {
            self.rng.pick(good).as_bytes().to_vec()
        }
    }

    fn maybe(&mut self, p: f64) -> bool {
        self.rng.chance(p)
    }

    fn domain(&mut self) -> String {
        let n = self.rng.range(1, 3);
        (0..n)
            .map(|_| self.rng.pick(LABELS))
            .collect::<Vec<_>>()
            .join(".")
    }

    fn host_pattern(&mut self) -> String {
        if self.fault() {
            let long_label = "l".repeat(64);
            let long_host = ["h"; 127].join(".");
            let bad: [&str; 15] = [
                "*.",
                "a..b",
                "-a.com",
                "a-.com",
                "a:80",
                "*a.com",
                "a.*.com",
                "a b",
                "*.*.a",
                "ä.com",
                "a.com.",
                &long_label,
                &long_host,
                "[::1]",
                "*.a_b",
            ];
            return self.rng.pick(&bad).to_owned();
        }
        let mut h = match self.rng.below(10) {
            0..=4 => self.domain(),
            5..=7 => format!("*.{}", self.domain()),
            8 => "*".to_owned(),
            _ => String::new(),
        };
        if self.maybe(0.15) {
            h = h.to_uppercase();
        }
        h
    }

    fn path_prefix(&mut self) -> String {
        if self.fault() {
            let long = format!("/{}", "p".repeat(1024));
            let bad: [&str; 16] = [
                "api", "/a//b", "/a/./b", "/a/../b", "/a?b", "/a%20", "/a b", "/a\\b", "/a#", "//",
                "/.", "/..", "/a\tb", "/a\u{7f}", &long, "/a/.",
            ];
            return self.rng.pick(&bad).to_owned();
        }
        if self.maybe(0.1) {
            return String::new();
        }
        let n = self.rng.range(0, 3);
        let mut p = String::from("/");
        for i in 0..n {
            if i > 0 {
                p.push('/');
            }
            p.push_str(self.rng.pick(SEGMENTS));
        }
        if n > 0 && self.maybe(0.3) {
            p.push('/');
        }
        if self.maybe(0.01) {
            // At the length limit exactly.
            p.push_str(&"q".repeat(1024 - p.len()));
        }
        p
    }

    fn duration(&mut self, good: &[&str], bad: &[&str]) -> Vec<u8> {
        if self.fault() && self.rng.chance(0.3) {
            let syntax = ["1x", "", "1.5.s", "ms", "9999999999h", "+-1s", "1µs", "1us"];
            return jstr(self.rng.pick(&syntax));
        }
        self.choose(good, bad)
    }

    fn cluster(&mut self, name: &str) -> Vec<u8> {
        let mut c = Obj::default();
        c.put("name", jstr(name));
        let mut maglev = false;
        if self.maybe(0.4) {
            let policy = self.choose(
                &["round_robin", "least_loaded", "maglev", ""],
                &["RoundRobin", "random", "MAGLEV"],
            );
            maglev = policy == b"\"maglev\"";
            c.put("policy", policy);
        }
        if maglev || self.fault() {
            let long = "h".repeat(65);
            let hh = self.choose(
                &["x-user-id", "X-User", "cookie", "a!#$%&'*+-.^_`|~"],
                &["", "bad header", "x:y", &long, "é"],
            );
            c.put("hash_header", hh);
        }
        let mut endpoints = Vec::new();
        let n = if self.fault() {
            self.rng.below(2) * 4
        } else {
            self.rng.range(1, 3)
        };
        let mut used = Vec::new();
        for _ in 0..n {
            let mut e = Obj::default();
            let addr = if self.fault() {
                self.rng.pick(&[
                    "h",
                    "h:0",
                    "a.-b:80",
                    "h:65536",
                    "[::1]",
                    "",
                    "h:08",
                    "h: 1",
                    "1.2.3.4.5:80",
                    "h:1",
                ])
            } else {
                let mut a = self.rng.pick(ADDRESSES);
                if used.contains(&a) {
                    a = ADDRESSES
                        [(ADDRESSES.iter().position(|x| *x == a).unwrap() + 1) % ADDRESSES.len()];
                }
                a
            };
            if used.contains(&addr) && self.fault_rate == 0.0 {
                continue;
            }
            used.push(addr);
            e.put("address", jstr(addr));
            if self.maybe(0.3) {
                let w = self.int(&["1", "3", "1000"], &["0", "1001", "-1"]);
                e.put("weight", w);
            }
            if self.maybe(0.2) {
                let p = self.int(&["0", "1", "7"], &["8", "-1"]);
                e.put("priority", p);
            }
            endpoints.push(e.render(self));
        }
        c.put("endpoints", array(endpoints));
        if self.maybe(0.2) {
            let d = self.duration(&["250ms", "1s", "1m", "1ms"], &["0s", "2m", "-1s", "999us"]);
            c.put("connect_timeout", d);
        }
        if self.maybe(0.2) {
            let d = self.duration(&["0s", "15s", "1h", "0"], &["1h1s", "-1ns"]);
            c.put("timeout", d);
        }
        if self.maybe(0.15) {
            let v = self.int(&["1", "1024", "1000000"], &["0", "1000001"]);
            c.put("max_requests", v);
        }
        if self.maybe(0.15) {
            let v = self.int(&["0", "50", "100"], &["101", "-1"]);
            c.put("panic_threshold_percent", v);
        }
        if self.maybe(0.15) {
            let mut h = Obj::default();
            let path = self.choose(&["/healthz", "/", "/a?b"], &["healthz", "/a b", "/#", ""]);
            h.put("path", path);
            if self.maybe(0.3) {
                let host = self.choose(&["h.example", "h:80", "10.0.0.1:1"], &["h:x", "-h", "h:0"]);
                h.put("host", host);
            }
            if self.maybe(0.4) {
                let d = self.duration(&["5s", "10ms", "1h"], &["9ms", "2h"]);
                h.put("interval", d);
            }
            if self.maybe(0.4) {
                let d = self.duration(&["1ms", "500ms"], &["0s", "10s"]);
                h.put("timeout", d);
            }
            if self.maybe(0.2) {
                let d = self.duration(&["0s", "100ms"], &["1h1s", "6s"]);
                h.put("jitter", d);
            }
            if self.maybe(0.2) {
                let v = self.int(&["1", "100"], &["0", "101"]);
                h.put("healthy_threshold", v);
            }
            let rendered = h.render(self);
            c.put("health_check", rendered);
        }
        if self.maybe(0.1) {
            let mut o = Obj::default();
            if self.maybe(0.5) {
                let v = self.int(&["0", "5", "1000"], &["1001", "-1"]);
                o.put("consecutive_gateway_failures", v);
            }
            if self.maybe(0.4) {
                let d = self.duration(&["30s", "1ms"], &["0s", "1h1ms"]);
                o.put("base_ejection_time", d);
            }
            if self.maybe(0.4) {
                let d = self.duration(&["300s", "24h"], &["1ms", "25h"]);
                o.put("max_ejection_time", d);
            }
            if self.maybe(0.3) {
                let v = self.int(&["0.5", "10", "1.9"], &["0", "10.5", "-1"]);
                o.put("success_rate_stdev_factor", v);
            }
            let rendered = o.render(self);
            c.put("outlier_detection", rendered);
        }
        if self.maybe(0.1) {
            let mut r = Obj::default();
            if self.maybe(0.4) {
                let v = self.int(&["0", "2", "5"], &["6", "-1"]);
                r.put("max_retries", v);
            }
            if self.maybe(0.3) {
                let v = self.int(
                    &["[502,503]", "[]", "[599]"],
                    &["[404]", "[502,502]", "[600]"],
                );
                r.put("retry_on", v);
            }
            if self.maybe(0.3) {
                let d = self.duration(&["0s", "1s"], &["2h", "16s"]);
                r.put("per_try_timeout", d);
            }
            if self.maybe(0.3) {
                let d = self.duration(&["10ms", "1m"], &["0s", "2m"]);
                r.put("backoff_max", d);
            }
            if self.maybe(0.3) {
                let v = self.int(&["0", "16777216"], &["-1", "16777217"]);
                r.put("max_body_bytes", v);
            }
            if self.maybe(0.3) {
                let mut b = Obj::default();
                let ratio = self.int(&["0", "0.5", "1"], &["1.5", "-0.1"]);
                b.put("ratio", ratio);
                let window = self.duration(&["1s", "10s", "1m"], &["1500ms", "2m", "500ms"]);
                b.put("window", window);
                let rendered = b.render(self);
                r.put("budget", rendered);
            }
            let rendered = r.render(self);
            c.put("retry", rendered);
        }
        c.render(self)
    }

    fn tls(&mut self) -> Vec<u8> {
        let mut t = Obj::default();
        let mut certs = Vec::new();
        let lo = usize::from(!self.fault());
        for _ in 0..self.rng.range(lo, 2) {
            let mut c = Obj::default();
            let cert = self.choose(&["c.pem", "/etc/c.pem"], &[""]);
            c.put("cert_file", cert);
            let key = self.choose(&["k.pem"], &[""]);
            c.put("key_file", key);
            if self.maybe(0.3) {
                let names = self.int(
                    &["[\"a.example\",\"*.B.example\"]", "[\"x\"]"],
                    &["[]", "[\"*\"]", "[\"a:1\"]", "[\"a..b\"]"],
                );
                c.put("server_names", names);
            }
            certs.push(c.render(self));
        }
        t.put("certificates", array(certs));
        if self.maybe(0.3) {
            let v = self.choose(&["1.2", "1.3", ""], &["1.1", "1.4", "TLS1.2"]);
            t.put("min_version", v);
        }
        if self.maybe(0.2) {
            let v = self.choose(&["modern", "compatible"], &["strong", "Modern"]);
            t.put("cipher_policy", v);
        }
        if self.maybe(0.2) {
            let v = self.int(
                &["[\"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256\",\"TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256\"]"],
                &[
                    "[]",
                    "[\"TLS_AES_128_GCM_SHA256\"]",
                    "[\"nope\"]",
                    "[\"TLS_RSA_WITH_RC4_128_SHA\"]",
                    "[\"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256\",\"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256\"]",
                ],
            );
            t.put("cipher_suites", v);
        }
        if self.maybe(0.2) {
            let d = self.duration(&["5s", "100ms", "1m"], &["99ms", "61s"]);
            t.put("handshake_timeout", d);
        }
        if self.maybe(0.2) {
            let v = self.int(&["1", "100000"], &["0", "100001"]);
            t.put("max_concurrent_handshakes", v);
        }
        if self.maybe(0.2) {
            t.put("strict_sni", b"true".to_vec());
        }
        t.render(self)
    }
}

/// Generates one configuration document from `rng`.
pub fn config(rng: Rng) -> GenConfig {
    let mut g = Gen {
        rng,
        fault_rate: 0.0,
    };
    g.fault_rate = g.rng.pick(&[0.0, 0.0, 0.0, 0.0, 0.01, 0.03, 0.08, 0.2]);
    let mut top = Obj::default();

    let listen = g.choose(
        &[
            ":8080",
            "0.0.0.0:80",
            "[::]:443",
            "localhost:0",
            "127.0.0.1:8080",
            ":0",
        ],
        &[
            "",
            "8080",
            ":65536",
            ":080",
            "host:80",
            "[::1]",
            "LOCALHOST:80",
            ":+80",
            "01.2.3.4:80",
            "[fe80::1%]:80",
            ":9901",
        ],
    );
    top.put("listen", listen);
    if g.maybe(0.3) {
        let v = g.choose(
            &["127.0.0.1:9901", ":9901", "[::1]:9901", ":0"],
            &[":8080", "x:1", ":", "[::]:443"],
        );
        top.put("admin_listen", v);
    }
    if g.maybe(0.2) {
        let mut items = Vec::new();
        for _ in 0..g.rng.range(0, 4) {
            items.push(g.choose(
                &[
                    "10.0.0.0/8",
                    "192.168.1.7",
                    "fd00::/8",
                    "::1",
                    "0.0.0.0/0",
                    "2001:db8::/32",
                    "::ffff:10.0.0.0/104",
                ],
                &[
                    "10.0.0.1/8",
                    "fe80::1%eth0",
                    "1.2.3",
                    "10.0.0.0/33",
                    "",
                    "10.0.0.0/08",
                ],
            ));
        }
        top.put("trusted_proxies", array(items));
    }
    if g.maybe(0.08) {
        let t = g.tls();
        top.put("tls", t);
    }

    // Clusters, with distinct names unless a fault says otherwise.
    let mut names: Vec<String> = Vec::new();
    let wanted = if g.fault() {
        g.rng.below(2) * 3
    } else {
        g.rng.range(1, 4)
    };
    for _ in 0..wanted {
        let name = if g.fault() {
            let long = "n".repeat(65);
            let bad = ["", "bad name", &long, "ünï", "a/b"];
            g.rng.pick(&bad).to_owned()
        } else {
            let n = g.rng.pick(CLUSTER_NAMES).to_owned();
            if names.contains(&n) && g.fault_rate == 0.0 {
                continue;
            }
            n
        };
        names.push(name);
    }
    let clusters: Vec<Vec<u8>> = names.iter().map(|n| g.cluster(n)).collect();

    // Routes over a small universe of hosts and prefixes, so that they
    // overlap and requests can aim at them.
    let host_pool: Vec<String> = (0..g.rng.range(1, 6)).map(|_| g.host_pattern()).collect();
    let prefix_pool: Vec<String> = (0..g.rng.range(1, 8)).map(|_| g.path_prefix()).collect();
    let route_count = match g.rng.below(10) {
        0..=5 => g.rng.range(1, 8),
        6..=8 => g.rng.range(9, 40),
        _ => g.rng.range(41, 200),
    };
    let route_count = if g.fault() {
        g.rng.below(2) * route_count
    } else {
        route_count
    };
    let mut routes = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    let (mut hosts, mut prefixes) = (Vec::new(), Vec::new());
    for _ in 0..route_count {
        let host = g
            .rng
            .pick(&host_pool.iter().map(String::as_str).collect::<Vec<_>>())
            .to_owned();
        let prefix = g
            .rng
            .pick(&prefix_pool.iter().map(String::as_str).collect::<Vec<_>>())
            .to_owned();
        let key = (
            if host.is_empty() {
                "*".to_owned()
            } else {
                host.to_lowercase()
            },
            if prefix.is_empty() {
                "/".to_owned()
            } else {
                prefix.clone()
            },
        );
        if seen.contains(&key) && !g.fault() {
            continue;
        }
        seen.push(key);
        let mut r = Obj::default();
        if !host.is_empty() || g.maybe(0.5) {
            r.put("host", jstr(&host));
        }
        if !prefix.is_empty() || g.maybe(0.5) {
            r.put("path_prefix", jstr(&prefix));
        }
        let cluster = if g.fault() || names.is_empty() {
            g.rng.pick(&["missing", "Web", ""]).to_owned()
        } else {
            g.rng
                .pick(&names.iter().map(String::as_str).collect::<Vec<_>>())
                .to_owned()
        };
        r.put("cluster", jstr(&cluster));
        routes.push(r.render(&mut g));
        hosts.push(host);
        prefixes.push(prefix);
    }
    top.put("routes", array(routes));
    top.put("clusters", array(clusters));

    let mut doc = top.render(&mut g);
    if g.fault() {
        mutate_document(&mut g.rng, &mut doc);
    }
    if g.rng.chance(0.002) {
        // Pad with whitespace to the size limit, or one byte past it.
        let target = MAX_FILE_BYTES + g.rng.below(2);
        if doc.len() < target {
            doc.resize(target, b' ');
        }
    }
    if hosts.is_empty() {
        hosts = host_pool;
    }
    if prefixes.is_empty() {
        prefixes = prefix_pool;
    }
    GenConfig {
        doc,
        hosts,
        prefixes,
    }
}

fn mutate_document(rng: &mut Rng, doc: &mut Vec<u8>) {
    match rng.below(9) {
        0 => {
            let at = rng.below(doc.len());
            doc.truncate(at);
        }
        1 => doc.extend_from_slice(
            rng.pick(&[" x", " {}", "\n", "  \t", "]", "null"])
                .as_bytes(),
        ),
        2 => {
            *doc = rng
                .pick(&["[]", "null", "1", "\"x\"", "{}", "", "{\"routes\":null}"])
                .as_bytes()
                .to_vec()
        }
        3 => {
            let at = rng.below(doc.len());
            doc[at] = rng.next_u64() as u8;
        }
        4 => {
            // Raw invalid UTF-8 inside the first string.
            if let Some(at) = doc.iter().position(|&c| c == b'"') {
                let bytes: &[u8] = rng.pick(&[&b"\xff"[..], b"\xc3", b"\xed\xa0\x80", b"\xe2\x82"]);
                doc.splice(at + 1..at + 1, bytes.iter().copied());
            }
        }
        5 => {
            // A \u escape, possibly an unpaired surrogate, inside a string.
            if let Some(at) = doc.iter().rposition(|&c| c == b'/') {
                let esc = rng.pick(&[
                    "\\ud800",
                    "\\u00e9",
                    "\\uDFFF",
                    "\\u002f",
                    "\\ud83d\\ude00",
                    "\\u0000",
                ]);
                doc.splice(at + 1..at + 1, esc.bytes());
            }
        }
        6 => doc
            .splice(0..0, b"\xef\xbb\xbf".iter().copied())
            .for_each(drop),
        7 => {
            // A BOM-less leading whitespace run and a comment-like prefix.
            doc.splice(0..0, rng.pick(&[" \n", "//c\n", "\u{a0}"]).bytes())
                .for_each(drop);
        }
        _ => {
            let at = rng.below(doc.len() + 1);
            let c = rng.pick(b",{\"\\");
            doc.insert(at, c);
        }
    }
}

/// Generates one request aimed at `cfg`'s routes.
pub fn request(rng: &mut Rng, cfg: &GenConfig) -> GenRequest {
    let host = request_host(rng, cfg);
    let mut target = request_target(rng, cfg);
    // A target that does not start with "/" must not look like it has a URL
    // scheme: steer's server parsed absolute-form targets for their path,
    // while RequestTarget takes only origin form and leaves absolute form
    // to the caller, so a scheme there is outside what is being compared.
    if target.first() != Some(&b'/') && target != b"*" {
        for c in target.iter_mut().filter(|c| **c == b':') {
            *c = b';';
        }
    }
    GenRequest { host, target }
}

fn random_domain(rng: &mut Rng) -> String {
    let n = rng.range(1, 3);
    (0..n)
        .map(|_| rng.pick(LABELS))
        .collect::<Vec<_>>()
        .join(".")
}

fn request_host(rng: &mut Rng, cfg: &GenConfig) -> String {
    let base = match rng.below(10) {
        0..=5 => {
            let p = &cfg.hosts[rng.below(cfg.hosts.len())];
            if p.is_empty() || p == "*" {
                random_domain(rng)
            } else if let Some(suffix) = p.strip_prefix("*.") {
                match rng.below(5) {
                    0 | 1 => format!("{}.{suffix}", rng.pick(LABELS)),
                    2 => suffix.to_owned(),
                    3 => format!("a.b.{suffix}"),
                    _ => format!(".{suffix}"),
                }
            } else {
                p.clone()
            }
        }
        6 | 7 => random_domain(rng),
        8 => rng
            .pick(&[
                "10.0.0.1",
                "10.0.0.1:80",
                "[::1]",
                "[::1]:8080",
                "::1",
                "[fe80::1%25eth0]:80",
                "1",
            ])
            .to_owned(),
        _ => rng
            .pick(&[
                "",
                " ",
                "a b",
                "ÄPI.EXAMPLE.COM",
                "\u{212a}.com",
                "ſhop.com",
                "a\u{130}.com",
                "a.com.:80",
                "[a.com]",
                "Exa\u{301}mple.com",
                "ΣΑΣ.net",
                ":80",
                "[]",
                "a.com:80:80",
                "\u{1e9e}.com",
            ])
            .to_owned(),
    };
    let mut h = base;
    if rng.chance(0.3) {
        h = h
            .chars()
            .map(|c| {
                if rng.chance(0.5) {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            })
            .collect();
    }
    if rng.chance(0.1) {
        h.push('.');
    }
    if rng.chance(0.05) {
        h = format!("[{h}]");
    }
    if rng.chance(0.2) {
        h.push_str(rng.pick(&[":80", ":", ":8080", ":0x", ":65536"]));
    }
    h
}

fn request_target(rng: &mut Rng, cfg: &GenConfig) -> Vec<u8> {
    let mut t: Vec<u8> = match rng.below(20) {
        0..=13 => {
            let p = &cfg.prefixes[rng.below(cfg.prefixes.len())];
            let mut t = if p.is_empty() {
                b"/".to_vec()
            } else {
                p.as_bytes().to_vec()
            };
            match rng.below(5) {
                0 => {}
                1 => {
                    t.pop();
                }
                2 => t.extend_from_slice(rng.pick(&["x", "-", ".", "%41", "é"]).as_bytes()),
                _ => {
                    for _ in 0..rng.range(1, 2) {
                        if t.last() != Some(&b'/') {
                            t.push(b'/');
                        }
                        t.extend_from_slice(rng.pick(SEGMENTS).as_bytes());
                    }
                    if rng.chance(0.3) {
                        t.push(b'/');
                    }
                }
            }
            t
        }
        14 => b"*".to_vec(),
        15 => rng
            .pick(&[
                &b"/"[..],
                b"//",
                b"/..",
                b"/../..",
                b"/%2e%2e/",
                b"/%",
                b"/%zz",
                b"/%4",
                b"/a%2",
                b"",
                b"a/b",
                b"?x",
                b"/a\x01",
                b"/\x7f",
                b"/a b",
                b"/\xff\xfe",
                b"/%ff",
                b"/%00",
                b"/%C3%A9",
                b"/\xc3\xa9",
                b"/a#b",
                b"/?",
                b"*?x",
                b"/%2F%2f",
                b"/%5C",
                b"/a/%2e/b",
            ])
            .to_vec(),
        _ => {
            let mut t = Vec::new();
            for _ in 0..rng.range(0, 4) {
                t.push(b'/');
                t.extend_from_slice(rng.pick(SEGMENTS).as_bytes());
            }
            if t.is_empty() || rng.chance(0.2) {
                t.push(b'/');
            }
            if rng.chance(0.1) {
                t.push((0x80 + rng.below(0x80)) as u8);
            }
            t
        }
    };
    for _ in 0..rng.below(3) {
        decorate(rng, &mut t);
    }
    t
}

/// One change of the kind that cleaning and decoding have to undo, or that
/// makes the target unparseable.
fn decorate(rng: &mut Rng, t: &mut Vec<u8>) {
    let slashes: Vec<usize> = t
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == b'/')
        .map(|(i, _)| i)
        .collect();
    match rng.below(10) {
        0 if !slashes.is_empty() => {
            let at = slashes[rng.below(slashes.len())];
            t.insert(at, b'/');
        }
        1 if !slashes.is_empty() => {
            let at = slashes[rng.below(slashes.len())];
            let ins: &[u8] = rng.pick(&[&b"/."[..], b"/..", b"/x/..", b"/%2e", b"/.%2E"]);
            t.splice(at..at, ins.iter().copied());
        }
        2 => t.extend_from_slice(rng.pick(&["/..", "/.", "/../", "/./", "//"]).as_bytes()),
        3 if !slashes.is_empty() => {
            let at = slashes[rng.below(slashes.len())];
            let esc: &[u8] = rng.pick(&[&b"%2F"[..], b"%2f", b"%5C"]);
            t.splice(at..=at, esc.iter().copied());
        }
        4 if !t.is_empty() => {
            // Percent-encode one byte, in either case of hex digit.
            let at = rng.below(t.len());
            let hex = if rng.chance(0.5) {
                format!("%{:02X}", t[at])
            } else {
                format!("%{:02x}", t[at])
            };
            t.splice(at..=at, hex.bytes());
        }
        5 => t.extend_from_slice(
            rng.pick(&["?q=1", "?", "?a/../b", "?%zz", "??", "?#"])
                .as_bytes(),
        ),
        6 => t.extend_from_slice(
            rng.pick(&["#f", "#", "%", "%a", "%g0", "\t", " ", "\x00"])
                .as_bytes(),
        ),
        7 if !t.is_empty() => {
            let at = rng.below(t.len());
            t[at] = t[at].to_ascii_uppercase();
        }
        8 if !t.is_empty() => {
            // Drop the leading slash, giving a target that is not in origin
            // form, or a path that only matches after cleaning adds it back.
            t.remove(0);
        }
        _ => t.push(b'/'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgefront_core::config::Config;

    #[test]
    fn documents_are_deterministic_per_seed() {
        let a = config(Rng::new(3));
        let b = config(Rng::new(3));
        assert_eq!(a.doc, b.doc);
        assert_eq!(a.hosts, b.hosts);
        let mut ra = Rng::new(4);
        let mut rb = Rng::new(4);
        assert_eq!(request(&mut ra, &a), request(&mut rb, &b));
    }

    #[test]
    fn about_half_of_documents_are_accepted_and_clean_ones_mostly_are() {
        let mut rng = Rng::new(11);
        let mut accepted = 0;
        let mut padded = 0;
        for _ in 0..2_000 {
            let c = config(rng.fork());
            assert!(!c.hosts.is_empty() && !c.prefixes.is_empty());
            padded += usize::from(c.doc.len() >= MAX_FILE_BYTES);
            accepted += usize::from(Config::from_json(&c.doc).is_ok());
        }
        assert!(
            (700..1_500).contains(&accepted),
            "{accepted} of 2000 accepted"
        );
        assert!(padded <= 20, "{padded}");
    }

    #[test]
    fn requests_cover_every_outcome() {
        use edgefront_proxy::path::RequestTarget;
        let mut rng = Rng::new(21);
        let (mut bad, mut cleaned, mut plain, mut colon) = (0, 0, 0, 0);
        for _ in 0..200 {
            let c = config(rng.fork());
            for _ in 0..20 {
                let r = request(&mut rng, &c);
                if r.target.first() != Some(&b'/') && r.target != b"*" && r.target.contains(&b':') {
                    colon += 1;
                }
                match RequestTarget::parse(&r.target) {
                    Err(_) => bad += 1,
                    Ok(t) if t.was_cleaned() => cleaned += 1,
                    Ok(_) => plain += 1,
                }
            }
        }
        assert_eq!(colon, 0);
        assert!(
            bad > 200 && cleaned > 200 && plain > 1_000,
            "{bad} {cleaned} {plain}"
        );
    }

    #[test]
    fn key_folds_change_only_case() {
        let mut rng = Rng::new(1);
        let folds: Vec<String> = (0..30).map(|_| fold_key(&mut rng, "path_prefix")).collect();
        assert!(folds.contains(&"PATH_PREFIX".to_owned()));
        assert!(folds.contains(&"Path_prefix".to_owned()));
        assert!(folds.contains(&"path_prefix".replace('s', "ſ")));
        assert_eq!(fold_key(&mut Rng::new(2), ""), "");
    }
}
