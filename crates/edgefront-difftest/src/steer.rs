//! Diffs edgefront's port of steer against steer itself.
//!
//! `legacy/steer/cmd/diffdrive` answers queries with steer's Go code over
//! line-delimited JSON on its stdin and stdout; [`answer`] answers the same
//! queries with the Rust crates. [`run`] generates configuration documents
//! and requests (see [`generate`]), asks both, and records every case where they
//! disagree.
//!
//! Two kinds of query are compared:
//!
//! - `validate`: whether a JSON document is accepted. A rejection also
//!   carries a class (`size`, `json` or `invalid`), and for `invalid` the
//!   whole error text, which lists every problem by field path in steer's
//!   words; all three must match.
//! - `route`: for an accepted document, the decision for each request. The
//!   target is parsed as an origin-form request target (`url.ParseRequestURI`
//!   in Go, [`RequestTarget::parse`] here), its path cleaned, and the Host
//!   and cleaned path matched against the routes. The outcome
//!   (`bad_request`, `no_route` or `route`), the cleaned path and the chosen
//!   route's host, prefix and cluster must all match.
//!
//! Byte strings cross the pipe as hex, so invalid UTF-8 survives the trip.

pub mod generate;

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use edgefront_core::config::{Config, ConfigError};
use edgefront_proxy::path::RequestTarget;
use edgefront_proxy::route::Router;
use serde::{Deserialize, Serialize};

use crate::rng::Rng;
pub use generate::{GenConfig, GenRequest};

/// The most requests sent in one route query.
const MAX_BATCH: usize = 1_000;
/// How many divergences a report keeps in full.
const MAX_EXAMPLES: usize = 20;

/// A question for both implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Is this JSON configuration accepted?
    Validate { config: Vec<u8> },
    /// How is each request decided under this configuration?
    Route {
        config: Vec<u8>,
        requests: Vec<GenRequest>,
    },
}

#[derive(Serialize, Deserialize)]
struct WireQuery {
    op: String,
    config: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requests: Vec<WireRequest>,
}

#[derive(Serialize, Deserialize)]
struct WireRequest {
    host: String,
    target: String,
}

impl Query {
    /// The query as one line of JSON, without the newline.
    pub fn to_line(&self) -> String {
        let wire = match self {
            Query::Validate { config } => WireQuery {
                op: "validate".into(),
                config: hex(config),
                requests: Vec::new(),
            },
            Query::Route { config, requests } => WireQuery {
                op: "route".into(),
                config: hex(config),
                requests: requests
                    .iter()
                    .map(|r| WireRequest {
                        host: r.host.clone(),
                        target: hex(&r.target),
                    })
                    .collect(),
            },
        };
        serde_json::to_string(&wire).expect("queries always serialise")
    }

    /// Parses a line written by [`Query::to_line`].
    pub fn from_line(line: &str) -> Result<Query, String> {
        let wire: WireQuery = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let config = unhex(&wire.config)?;
        match wire.op.as_str() {
            "validate" => Ok(Query::Validate { config }),
            "route" => Ok(Query::Route {
                config,
                requests: wire
                    .requests
                    .into_iter()
                    .map(|r| {
                        Ok(GenRequest {
                            host: r.host,
                            target: unhex(&r.target)?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            }),
            op => Err(format!("unknown op {op:?}")),
        }
    }
}

/// An implementation's answer to a [`Query`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<Decision>,
}

/// How one request was decided. `path` is the cleaned path in hex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path_prefix: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cluster: String,
}

/// Answers a query with edgefront's crates.
pub fn answer(q: &Query) -> Answer {
    let (doc, requests) = match q {
        Query::Validate { config } => (config, None),
        Query::Route { config, requests } => (config, Some(requests)),
    };
    let cfg = match Config::from_json(doc) {
        Ok(cfg) => cfg,
        Err(e) => {
            let class = match &e {
                ConfigError::TooLarge { .. } => "size",
                ConfigError::Syntax { .. } | ConfigError::Decode { .. } => "json",
                ConfigError::Invalid { .. } => "invalid",
                ConfigError::Io { .. } => "other",
            };
            return Answer {
                ok: false,
                class: class.into(),
                error: e.to_string(),
                results: Vec::new(),
            };
        }
    };
    let Some(requests) = requests else {
        return Answer {
            ok: true,
            ..Answer::default()
        };
    };
    let router = Router::new(
        cfg.routes
            .iter()
            .map(|r| (r.host.as_str(), r.path_prefix.as_str(), r)),
    );
    let results = requests
        .iter()
        .map(|req| {
            let Ok(target) = RequestTarget::parse(&req.target) else {
                return Decision {
                    status: "bad_request".into(),
                    ..Decision::default()
                };
            };
            let path = target.clean_path();
            match router.route(&req.host, path) {
                None => Decision {
                    status: "no_route".into(),
                    path: hex(path),
                    ..Decision::default()
                },
                Some(r) => Decision {
                    status: "route".into(),
                    path: hex(path),
                    host: r.host.clone(),
                    path_prefix: r.path_prefix.clone(),
                    cluster: r.cluster.clone(),
                },
            }
        })
        .collect();
    Answer {
        ok: true,
        results,
        ..Answer::default()
    }
}

/// Serves the diffdrive protocol from edgefront's crates: one answer line
/// per query line, until `input` ends. This is the Rust twin of
/// `cmd/diffdrive`, used to test the harness end to end. `picks` queries,
/// written by [`Scenario::to_line`](crate::balancer::Scenario::to_line),
/// are answered by [`balancer::answer_line`](crate::balancer::answer_line).
pub fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    for (n, line) in input.lines().enumerate() {
        let line = line?;
        let bad =
            |e: String| io::Error::new(io::ErrorKind::InvalidData, format!("line {}: {e}", n + 1));
        if line.starts_with(r#"{"op":"picks""#) {
            let a = crate::balancer::answer_line(&line).map_err(bad)?;
            serde_json::to_writer(&mut output, &a)?;
            output.write_all(b"\n")?;
            output.flush()?;
            continue;
        }
        let q = Query::from_line(&line).map_err(bad)?;
        serde_json::to_writer(&mut output, &answer(&q))?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

/// Something that answers queries: steer's driver, or a stand-in in tests.
pub trait Oracle {
    fn ask(&mut self, q: &Query) -> io::Result<Answer>;
}

/// A driver process speaking the protocol on its stdin and stdout.
#[derive(Debug)]
pub struct Driver {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    line: String,
}

impl Driver {
    /// Starts `program` with `args`. Its stderr is passed through.
    pub fn spawn(program: &str, args: &[String]) -> io::Result<Driver> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().map(BufWriter::new);
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Ok(Driver {
            child,
            stdin,
            stdout,
            line: String::new(),
        })
    }
}

impl Driver {
    /// Sends one query line and returns the answer line, with its newline.
    pub fn ask_line(&mut self, query: &str) -> io::Result<String> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        stdin.write_all(query.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        self.line.clear();
        if self.stdout.read_line(&mut self.line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the driver exited without answering",
            ));
        }
        Ok(std::mem::take(&mut self.line))
    }
}

impl Oracle for Driver {
    fn ask(&mut self, q: &Query) -> io::Result<Answer> {
        self.line = self.ask_line(&q.to_line())?;
        serde_json::from_str(&self.line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad answer {:?}: {e}", self.line.trim_end()),
            )
        })
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Closing stdin ends the driver's loop.
        self.stdin.take();
        let _ = self.child.wait();
    }
}

/// How much to generate, and from which seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub configs: usize,
    pub requests: usize,
    pub seed: u64,
}

/// One case where the implementations disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Which case: `config 17`, or `config 17 request 3`.
    pub case: String,
    /// The input and both answers, for a person to read.
    pub detail: String,
}

/// What a run compared, and where the implementations disagreed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub seed: u64,
    pub configs: usize,
    pub accepted: usize,
    /// Rejected documents by class.
    pub rejected: BTreeMap<String, usize>,
    pub requests: usize,
    /// Request decisions by status.
    pub outcomes: BTreeMap<String, usize>,
    /// The number of divergent cases.
    pub divergences: usize,
    /// The first divergences, in full.
    pub examples: Vec<Divergence>,
}

impl Report {
    fn diverge(&mut self, case: String, detail: String) {
        self.divergences += 1;
        if self.examples.len() < MAX_EXAMPLES {
            self.examples.push(Divergence { case, detail });
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let join = |m: &BTreeMap<String, usize>| {
            m.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(f, "seed {}", self.seed)?;
        writeln!(
            f,
            "configs: {} ({} accepted; rejected: {})",
            self.configs,
            self.accepted,
            join(&self.rejected)
        )?;
        writeln!(f, "requests: {} ({})", self.requests, join(&self.outcomes))?;
        write!(f, "divergences: {}", self.divergences)?;
        for d in &self.examples {
            write!(f, "\n\n{}\n{}", d.case, d.detail)?;
        }
        if self.divergences > self.examples.len() {
            write!(
                f,
                "\n\n({} more divergences)",
                self.divergences - self.examples.len()
            )?;
        }
        Ok(())
    }
}

/// Generates `plan.configs` documents and validates each with both
/// implementations, then spreads `plan.requests` requests evenly over the
/// documents both accepted and routes them with both.
pub fn run(oracle: &mut dyn Oracle, plan: Plan) -> io::Result<Report> {
    let mut report = Report {
        seed: plan.seed,
        ..Report::default()
    };
    let mut rng = Rng::new(plan.seed);
    let mut accepted: Vec<(usize, GenConfig, Rng)> = Vec::new();
    for i in 0..plan.configs {
        let cfg = generate::config(rng.fork());
        let request_rng = rng.fork();
        let q = Query::Validate {
            config: cfg.doc.clone(),
        };
        let theirs = oracle.ask(&q)?;
        let ours = answer(&q);
        report.configs += 1;
        if !same_verdict(&theirs, &ours) {
            report.diverge(
                format!("config {i}"),
                describe_config(&cfg.doc, &theirs, &ours),
            );
            continue;
        }
        if ours.ok {
            report.accepted += 1;
            accepted.push((i, cfg, request_rng));
        } else {
            *report.rejected.entry(ours.class).or_default() += 1;
        }
    }
    if accepted.is_empty() {
        return Ok(report);
    }
    let per = plan.requests / accepted.len();
    let extra = plan.requests % accepted.len();
    for (n, (i, cfg, mut rng)) in accepted.into_iter().enumerate() {
        let count = per + usize::from(n < extra);
        let requests: Vec<GenRequest> = (0..count)
            .map(|_| generate::request(&mut rng, &cfg))
            .collect();
        for (b, batch) in requests.chunks(MAX_BATCH).enumerate() {
            let q = Query::Route {
                config: cfg.doc.clone(),
                requests: batch.to_vec(),
            };
            let theirs = oracle.ask(&q)?;
            let ours = answer(&q);
            if !same_verdict(&theirs, &ours) || theirs.results.len() != batch.len() {
                report.diverge(
                    format!("config {i} routes"),
                    describe_config(&cfg.doc, &theirs, &ours),
                );
                continue;
            }
            for (k, (req, (t, o))) in batch
                .iter()
                .zip(theirs.results.iter().zip(&ours.results))
                .enumerate()
            {
                report.requests += 1;
                *report.outcomes.entry(o.status.clone()).or_default() += 1;
                if t != o {
                    report.diverge(
                        format!("config {i} request {}", b * MAX_BATCH + k),
                        format!(
                            "config: {}\nhost: {:?}\ntarget: {}\nsteer: {}\nedgefront: {}",
                            show_doc(&cfg.doc),
                            req.host,
                            show_bytes(&req.target),
                            json(t),
                            json(o)
                        ),
                    );
                }
            }
        }
    }
    Ok(report)
}

/// Whether two answers agree on a document: the same acceptance, the same
/// class of rejection, and for validation failures the same problems.
fn same_verdict(theirs: &Answer, ours: &Answer) -> bool {
    theirs.ok == ours.ok
        && (theirs.ok
            || (theirs.class == ours.class
                && (theirs.class != "invalid" || theirs.error == ours.error)))
}

fn describe_config(doc: &[u8], theirs: &Answer, ours: &Answer) -> String {
    let brief = |a: &Answer| Answer {
        results: Vec::new(),
        ..a.clone()
    };
    format!(
        "config: {}\nsteer: {}\nedgefront: {}",
        show_doc(doc),
        json(&brief(theirs)),
        json(&brief(ours))
    )
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("answers always serialise")
}

/// A document for a report: as text, with invalid UTF-8 escaped, cut short
/// if it is long.
fn show_doc(doc: &[u8]) -> String {
    const LIMIT: usize = 4_000;
    if doc.len() > LIMIT {
        format!("{} ... ({} bytes)", show_bytes(&doc[..LIMIT]), doc.len())
    } else {
        show_bytes(doc)
    }
}

fn show_bytes(b: &[u8]) -> String {
    b.utf8_chunks()
        .map(|c| {
            let mut s: String = c.valid().escape_debug().collect();
            for byte in c.invalid() {
                s.push_str(&format!("\\x{byte:02x}"));
            }
            s
        })
        .collect()
}

/// Lower-case hex, as Go's `hex.EncodeToString` writes it.
pub fn hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &c in b {
        s.push(DIGITS[usize::from(c >> 4)] as char);
        s.push(DIGITS[usize::from(c & 15)] as char);
    }
    s
}

/// Decodes hex in either case.
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit {:?}", c as char)),
    };
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err("odd length hex".into());
    }
    s.chunks(2)
        .map(|p| Ok(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers with the Rust crates, optionally changed by `tamper`.
    struct Local<F: FnMut(&Query, &mut Answer)>(F);

    impl<F: FnMut(&Query, &mut Answer)> Oracle for Local<F> {
        fn ask(&mut self, q: &Query) -> io::Result<Answer> {
            let mut a = answer(q);
            (self.0)(q, &mut a);
            Ok(a)
        }
    }

    const DOC: &str = r#"{"listen":":8080","routes":[{"host":"*.Example.com","path_prefix":"/api/","cluster":"api"},{"cluster":"web"}],
        "clusters":[{"name":"api","endpoints":[{"address":"h:1"}]},{"name":"web","endpoints":[{"address":"h:2"}]}]}"#;

    fn req(host: &str, target: &[u8]) -> GenRequest {
        GenRequest {
            host: host.into(),
            target: target.to_vec(),
        }
    }

    #[test]
    fn answers_validation_and_routing() {
        let a = answer(&Query::Validate { config: DOC.into() });
        assert_eq!(
            a,
            Answer {
                ok: true,
                ..Answer::default()
            }
        );

        let a = answer(&Query::Route {
            config: DOC.into(),
            requests: vec![
                req("A.example.com:80", b"/x/../api/v1?q"),
                req("example.com", b"/api/"),
                req("a.example.com", b"/%zz"),
                req("a.example.com", b"/apx"),
            ],
        });
        let r = &a.results;
        assert_eq!(r.len(), 4);
        assert_eq!(
            r[0],
            Decision {
                status: "route".into(),
                path: hex(b"/api/v1"),
                host: "*.example.com".into(),
                path_prefix: "/api/".into(),
                cluster: "api".into(),
            }
        );
        assert_eq!(
            (
                r[1].status.as_str(),
                r[1].cluster.as_str(),
                r[1].host.as_str()
            ),
            ("route", "web", "*")
        );
        assert_eq!(
            r[2],
            Decision {
                status: "bad_request".into(),
                ..Decision::default()
            }
        );
        assert_eq!(
            r[3],
            Decision {
                status: "no_route".into(),
                path: hex(b"/apx"),
                ..Decision::default()
            }
        );
    }

    #[test]
    fn classifies_rejections() {
        let class = |doc: &[u8]| {
            answer(&Query::Validate {
                config: doc.to_vec(),
            })
        };
        let a = class(b"{");
        assert_eq!((a.ok, a.class.as_str()), (false, "json"));
        assert_eq!(class(br#"{"listen":5}"#).class, "json");
        let a = class(b"{}");
        assert_eq!(a.class, "invalid");
        assert!(
            a.error
                .starts_with("invalid configuration:\nlisten: is required"),
            "{}",
            a.error
        );
        assert_eq!(class(&vec![b' '; (1 << 20) + 1]).class, "size");
        // A rejected route query has no results.
        let a = answer(&Query::Route {
            config: b"{}".to_vec(),
            requests: vec![req("h", b"/")],
        });
        assert!(!a.ok && a.results.is_empty());
    }

    #[test]
    fn queries_round_trip_through_their_wire_form() {
        let qs = [
            Query::Validate {
                config: b"\xff{}".to_vec(),
            },
            Query::Route {
                config: DOC.into(),
                requests: vec![req("h\u{212a}", b"/\x00\xff"), req("", b"")],
            },
            Query::Route {
                config: Vec::new(),
                requests: Vec::new(),
            },
        ];
        for q in qs {
            let line = q.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(Query::from_line(&line), Ok(q));
        }
        assert_eq!(
            Query::from_line(r#"{"op":"nope","config":""}"#),
            Err("unknown op \"nope\"".into())
        );
        assert!(Query::from_line(r#"{"op":"validate","config":"zz"}"#).is_err());
        assert!(
            Query::from_line(r#"{"op":"route","config":"","requests":[{"host":"","target":"0"}]}"#)
                .is_err()
        );
        assert!(Query::from_line("[").is_err());
    }

    #[test]
    fn hex_matches_go_and_rejects_garbage() {
        assert_eq!(hex(b"\x00\x7f\xab/"), "007fab2f");
        assert_eq!(unhex("007FaB2f"), Ok(b"\x00\x7f\xab/".to_vec()));
        assert_eq!(unhex("abc"), Err("odd length hex".into()));
        assert_eq!(unhex("0g"), Err("invalid hex digit 'g'".into()));
    }

    #[test]
    fn serve_answers_each_line_and_stops_on_garbage() {
        let input = format!(
            "{}\n{}\n",
            Query::Validate { config: DOC.into() }.to_line(),
            Query::Route {
                config: DOC.into(),
                requests: vec![req("x", b"/")]
            }
            .to_line()
        );
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out).unwrap();
        let answers: Vec<Answer> = out
            .split(|&c| c == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[1].results[0].cluster, "web");

        let err = serve(
            &b"{\"op\":\"validate\",\"config\":\"\"}\nnot json\n"[..],
            io::sink(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().starts_with("line 2: "), "{err}");
    }

    #[test]
    fn identical_implementations_never_diverge() {
        let plan = Plan {
            configs: 300,
            requests: 3_001,
            seed: 99,
        };
        let report = run(&mut Local(|_: &Query, _: &mut Answer| {}), plan).unwrap();
        assert_eq!(report.divergences, 0, "{report}");
        assert_eq!(report.configs, 300);
        assert_eq!(report.requests, 3_001);
        assert_eq!(
            report.accepted + report.rejected.values().sum::<usize>(),
            300
        );
        for status in ["bad_request", "no_route", "route"] {
            assert!(
                report.outcomes.get(status).copied().unwrap_or(0) > 100,
                "{report}"
            );
        }
        for class in ["json", "invalid"] {
            assert!(report.rejected.contains_key(class), "{report}");
        }
        // The same seed generates the same run.
        assert_eq!(
            run(&mut Local(|_: &Query, _: &mut Answer| {}), plan).unwrap(),
            report
        );
        let text = report.to_string();
        assert!(text.starts_with("seed 99\nconfigs: 300 ("), "{text}");
        assert!(text.ends_with("divergences: 0"), "{text}");
    }

    #[test]
    fn disagreements_on_documents_are_reported() {
        // Flip the acceptance of some documents and change the error text
        // of some validation failures.
        let mut n = 0;
        let tamper = move |q: &Query, a: &mut Answer| {
            n += 1;
            if let Query::Validate { .. } = q {
                if n % 7 == 0 {
                    a.ok = !a.ok;
                } else if !a.ok && a.class == "invalid" && n % 3 == 0 {
                    a.error.push('!');
                }
            }
        };
        let report = run(
            &mut Local(tamper),
            Plan {
                configs: 300,
                requests: 300,
                seed: 5,
            },
        )
        .unwrap();
        assert!(report.divergences > 40, "{report}");
        assert_eq!(report.examples.len(), MAX_EXAMPLES);
        assert!(
            report
                .examples
                .iter()
                .all(|d| d.case.starts_with("config "))
        );
        let text = report.to_string();
        assert!(text.contains("more divergences)"), "{text}");
        assert!(text.contains("\nsteer: {\"ok\":true"), "{text}");
        assert!(text.contains("!\"}\nedgefront: {\"ok\":false"), "{text}");
    }

    #[test]
    fn disagreements_on_routes_are_reported() {
        // Drop the last decision from every third route answer and change
        // the first decision of the others.
        let mut n = 0;
        let tamper = move |q: &Query, a: &mut Answer| {
            if let Query::Route { .. } = q {
                n += 1;
                if n % 3 == 0 {
                    a.results.pop();
                } else if let Some(d) = a.results.first_mut() {
                    d.path.push_str("00");
                }
            }
        };
        let report = run(
            &mut Local(tamper),
            Plan {
                configs: 60,
                requests: 600,
                seed: 8,
            },
        )
        .unwrap();
        let routes = report
            .examples
            .iter()
            .filter(|d| d.case.ends_with(" routes"))
            .count();
        let requests = report
            .examples
            .iter()
            .filter(|d| d.case.contains(" request "))
            .count();
        assert!(routes > 0 && requests > 0, "{report}");
        assert!(report.divergences >= routes + requests);
        let text = report.to_string();
        assert!(text.contains("\nhost: \""), "{text}");
        assert!(text.contains("\ntarget: "), "{text}");
    }

    #[test]
    fn a_run_with_nothing_accepted_routes_nothing() {
        let reject = |_: &Query, a: &mut Answer| {
            *a = Answer {
                ok: false,
                class: "json".into(),
                ..Answer::default()
            };
        };
        // Every document diverges or is rejected, so nothing is routed.
        let report = run(
            &mut Local(reject),
            Plan {
                configs: 20,
                requests: 100,
                seed: 1,
            },
        )
        .unwrap();
        assert_eq!((report.accepted, report.requests), (0, 0));
        assert_eq!(report.configs, 20);
    }

    #[test]
    fn reports_show_bytes_readably() {
        assert_eq!(show_bytes(b"a\"\n\xffb"), "a\\\"\\n\\xffb");
        let long = vec![b'x'; 5_000];
        assert!(show_doc(&long).ends_with(" ... (5000 bytes)"));
    }

    #[test]
    fn a_driver_that_exits_or_babbles_is_an_error() {
        let q = Query::Validate {
            config: b"{}".to_vec(),
        };
        let mut silent = Driver::spawn("true", &[]).unwrap();
        let err = silent.ask(&q).unwrap_err();
        // Either the write hits a closed pipe or the read sees the end.
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
            ),
            "{err}"
        );

        let mut babbler = Driver::spawn(
            "sh",
            &["-c".into(), "read l; echo nonsense; cat >/dev/null".into()],
        )
        .unwrap();
        let err = babbler.ask(&q).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("bad answer \"nonsense\""), "{err}");

        assert!(Driver::spawn("/nonexistent/diffdrive", &[]).is_err());
    }
}
