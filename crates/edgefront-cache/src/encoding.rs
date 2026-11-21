//! Accept-Encoding (RFC 9110 section 12.5.3), reduced to the one content
//! coding a response from the cache will use.
//!
//! A request is mapped to exactly one of identity, gzip, br and zstd, so the
//! cache keeps at most four encoded variants of a URL however clients spell
//! the header: `gzip, deflate, br` and `br;q=1.0, gzip;q=0.5` both normalise
//! to `br`. The rules:
//!
//! - Each member is a coding with an optional `q` weight from 0 to 1 (at
//!   most three decimals); `q=0` means not acceptable. A member that does not
//!   parse is skipped, never the whole field, and names are matched in any
//!   case.
//! - `*` stands for every coding the field does not name.
//! - `identity` is acceptable unless the field gives it, or `*` without it,
//!   a weight of 0.
//! - No Accept-Encoding means any coding is acceptable; the cache then
//!   answers with identity, which every client can read.
//! - An empty field means identity only.
//! - Among the acceptable codings the highest weight wins, and a tie goes
//!   to the server's preference, best compression first: br, zstd, gzip,
//!   identity. `x-gzip` is gzip (RFC 9110 section 8.4.1.3).
//!
//! When no coding the cache can serve is acceptable, [`negotiate`] says so
//! and the request should be answered with 406 or sent to the origin.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::header::{self, HeaderMap};

/// The content codings the cache serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Coding {
    Identity,
    Gzip,
    Br,
    Zstd,
}

impl Coding {
    /// Every coding, in the server's order of preference for ties.
    pub const PREFERENCE: [Coding; 4] = [Coding::Br, Coding::Zstd, Coding::Gzip, Coding::Identity];

    /// The token in Content-Encoding (none for identity).
    pub fn as_str(self) -> &'static str {
        match self {
            Coding::Identity => "identity",
            Coding::Gzip => "gzip",
            Coding::Br => "br",
            Coding::Zstd => "zstd",
        }
    }

    /// The coding of a stored or upstream response, from its
    /// Content-Encoding: identity when there is none, `None` when it is
    /// one the cache does not negotiate (`deflate`, a dictionary coding,
    /// or several codings applied in turn).
    pub fn of_response(headers: &HeaderMap) -> Option<Coding> {
        let mut codings = crate::headers::field_list(headers, &header::CONTENT_ENCODING);
        match codings.len() {
            0 => Some(Coding::Identity),
            1 => Coding::from_token(&codings.remove(0)),
            _ => None,
        }
    }

    fn from_token(token: &str) -> Option<Coding> {
        [
            ("identity", Coding::Identity),
            ("gzip", Coding::Gzip),
            ("x-gzip", Coding::Gzip),
            ("br", Coding::Br),
            ("zstd", Coding::Zstd),
        ]
        .into_iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(token))
        .map(|(_, c)| c)
    }
}

/// A weight in thousandths, 0 to 1000.
type Q = u16;

/// Parses a weight: `0`, `1`, or `0.` / `1.` followed by up to three
/// digits, `1.` only by zeros (RFC 9110 section 12.4.2).
pub(crate) fn parse_q(v: &str) -> Option<Q> {
    let (int, frac) = match v.split_once('.') {
        Some((i, f)) => (i, f),
        None => (v, ""),
    };
    if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let thousandths = format!("{frac:0<3}").parse::<Q>().ok()?;
    match int {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// What a request's Accept-Encoding allows: the weight of each coding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    weights: [Q; 4],
}

fn index(c: Coding) -> usize {
    match c {
        Coding::Identity => 0,
        Coding::Gzip => 1,
        Coding::Br => 2,
        Coding::Zstd => 3,
    }
}

impl Accepted {
    /// Reads every Accept-Encoding field line of `headers`.
    pub fn from_headers(headers: &HeaderMap) -> Accepted {
        let mut lines = headers.get_all(header::ACCEPT_ENCODING).iter().peekable();
        if lines.peek().is_none() {
            // No preference: anything goes, identity first among equals.
            return Accepted { weights: [1000; 4] };
        }
        let mut named: [Option<Q>; 4] = [None; 4];
        let mut star: Option<Q> = None;
        for line in lines {
            let Ok(text) = line.to_str() else {
                continue;
            };
            for member in text.split(',') {
                let mut parts = member.split(';');
                let token = parts.next().unwrap_or("").trim();
                if token.is_empty() {
                    continue;
                }
                let mut q = Some(1000);
                for param in parts {
                    let Some((k, v)) = param.split_once('=') else {
                        q = None;
                        break;
                    };
                    if k.trim().eq_ignore_ascii_case("q") {
                        q = parse_q(v.trim());
                    }
                }
                let Some(q) = q else {
                    continue;
                };
                if token == "*" {
                    star.get_or_insert(q);
                } else if let Some(c) = Coding::from_token(token) {
                    named[index(c)].get_or_insert(q);
                }
            }
        }
        let mut weights = [0; 4];
        for c in Coding::PREFERENCE {
            let i = index(c);
            weights[i] = match (named[i], star, c) {
                (Some(q), _, _) => q,
                (None, Some(q), _) => q,
                // Identity is acceptable unless excluded.
                (None, None, Coding::Identity) => 1,
                (None, None, _) => 0,
            };
        }
        Accepted { weights }
    }

    /// The weight given to `c`, in thousandths.
    pub fn weight(&self, c: Coding) -> u16 {
        self.weights[index(c)]
    }

    /// The best of `available` this request accepts: highest weight, ties
    /// to the server's preference. `None` when it accepts none of them.
    pub fn best(&self, available: &[Coding]) -> Option<Coding> {
        Coding::PREFERENCE
            .into_iter()
            .filter(|c| available.contains(c) && self.weight(*c) > 0)
            .max_by(|a, b| {
                self.weight(*a).cmp(&self.weight(*b)).then_with(|| {
                    // Earlier in PREFERENCE wins the tie.
                    let pos = |c: &Coding| Coding::PREFERENCE.iter().position(|p| p == c);
                    pos(b).cmp(&pos(a))
                })
            })
    }
}

/// The coding to answer `headers` with, among `available`.
pub fn negotiate(headers: &HeaderMap, available: &[Coding]) -> Option<Coding> {
    let accepted = Accepted::from_headers(headers);
    // No Accept-Encoding at all: identity, which every client reads.
    if !headers.contains_key(header::ACCEPT_ENCODING) && available.contains(&Coding::Identity) {
        return Some(Coding::Identity);
    }
    accepted.best(available)
}

/// Whether a request accepts a response in `coding`: always when it has
/// no Accept-Encoding, else when the field gives the coding a weight.
pub fn acceptable(headers: &HeaderMap, coding: Coding) -> bool {
    !headers.contains_key(header::ACCEPT_ENCODING)
        || Accepted::from_headers(headers).weight(coding) > 0
}

/// The normalised Accept-Encoding a request is cached under: the coding it
/// would be served among all four, or `None` when it accepts none.
pub fn normalise(headers: &HeaderMap) -> Option<Coding> {
    negotiate(headers, &Coding::PREFERENCE)
}

/// The codings compression is counted under: the four served ones but
/// identity, and RFC 9842's two dictionary codings.
pub const COUNTED: [&str; 5] = ["br", "zstd", "gzip", "dcb", "dcz"];

/// Upper bounds of the compression ratio buckets: compressed bytes over
/// source bytes.
pub const RATIO_BUCKETS: [f64; 9] = [0.02, 0.05, 0.1, 0.2, 0.3, 0.4, 0.6, 0.8, 1.0];

#[derive(Debug, Default)]
struct CodingStats {
    jobs: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    nanos: AtomicU64,
    ratio: [AtomicU64; RATIO_BUCKETS.len() + 1],
    ratio_sum_millis: AtomicU64,
}

/// What compression cost and saved, per coding, and what it skipped
/// because the blocking pool was over its budget. The time is the time
/// spent compressing on a blocking-pool thread, which compression keeps
/// busy, so it stands for the CPU it used.
#[derive(Debug, Default)]
pub struct CompressionStats {
    codings: [CodingStats; COUNTED.len()],
    skipped: AtomicU64,
}

impl CompressionStats {
    fn of(&self, coding: &str) -> Option<&CodingStats> {
        COUNTED
            .iter()
            .position(|c| *c == coding)
            .map(|i| &self.codings[i])
    }

    /// Records one compression of `bytes_in` into `bytes_out` in `took`.
    pub fn record(&self, coding: &str, bytes_in: usize, bytes_out: usize, took: Duration) {
        let Some(c) = self.of(coding) else { return };
        c.jobs.fetch_add(1, Ordering::Relaxed);
        c.bytes_in.fetch_add(bytes_in as u64, Ordering::Relaxed);
        c.bytes_out.fetch_add(bytes_out as u64, Ordering::Relaxed);
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        c.nanos.fetch_add(nanos, Ordering::Relaxed);
        let ratio = bytes_out as f64 / bytes_in.max(1) as f64;
        let i = RATIO_BUCKETS
            .iter()
            .position(|&b| ratio <= b)
            .unwrap_or(RATIO_BUCKETS.len());
        c.ratio[i].fetch_add(1, Ordering::Relaxed);
        c.ratio_sum_millis
            .fetch_add((ratio * 1000.0).round() as u64, Ordering::Relaxed);
    }

    /// Counts one compression not done because the pool was busy.
    pub fn skip(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Compressions skipped for the budget.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// Source bytes compressed in `coding`, and the bytes that came out.
    pub fn bytes(&self, coding: &str) -> (u64, u64) {
        self.of(coding).map_or((0, 0), |c| {
            (
                c.bytes_in.load(Ordering::Relaxed),
                c.bytes_out.load(Ordering::Relaxed),
            )
        })
    }

    /// Microseconds of compression per MiB of source in `coding`, `None`
    /// before any.
    pub fn micros_per_mib(&self, coding: &str) -> Option<f64> {
        let c = self.of(coding)?;
        let bytes = c.bytes_in.load(Ordering::Relaxed);
        (bytes > 0).then(|| {
            c.nanos.load(Ordering::Relaxed) as f64 / 1e3 / (bytes as f64 / (1 << 20) as f64)
        })
    }

    /// Writes the compression families in the Prometheus text format under
    /// `prefix` (`cachet` makes `cachet_compression_...`).
    pub fn render(&self, out: &mut String, prefix: &str) {
        use std::fmt::Write as _;
        let fam = |out: &mut String, name: &str, help: &str, kind: &str| {
            let _ = writeln!(
                out,
                "# HELP {prefix}_{name} {help}\n# TYPE {prefix}_{name} {kind}"
            );
        };
        let per = |out: &mut String,
                   name: &str,
                   help: &str,
                   kind: &str,
                   value: &dyn Fn(&CodingStats) -> String| {
            fam(out, name, help, kind);
            for (coding, c) in COUNTED.iter().zip(&self.codings) {
                let _ = writeln!(out, "{prefix}_{name}{{coding=\"{coding}\"}} {}", value(c));
            }
        };
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        per(
            out,
            "compression_jobs_total",
            "Bodies compressed, by coding.",
            "counter",
            &|c| load(&c.jobs).to_string(),
        );
        per(
            out,
            "compression_input_bytes_total",
            "Source bytes compressed, by coding.",
            "counter",
            &|c| load(&c.bytes_in).to_string(),
        );
        per(
            out,
            "compression_output_bytes_total",
            "Compressed bytes produced, by coding.",
            "counter",
            &|c| load(&c.bytes_out).to_string(),
        );
        per(
            out,
            "compression_seconds_total",
            "Time spent compressing on blocking-pool threads, by coding.",
            "counter",
            &|c| format!("{}", load(&c.nanos) as f64 / 1e9),
        );
        fam(
            out,
            "compression_cpu_microseconds_per_mib",
            "Compression time per MiB of source, by coding, since start.",
            "gauge",
        );
        for coding in COUNTED {
            let v = self.micros_per_mib(coding).unwrap_or(0.0);
            let _ = writeln!(
                out,
                "{prefix}_compression_cpu_microseconds_per_mib{{coding=\"{coding}\"}} {v:.1}"
            );
        }
        fam(
            out,
            "compression_ratio",
            "Compressed size over source size, by coding.",
            "histogram",
        );
        for (coding, c) in COUNTED.iter().zip(&self.codings) {
            let mut cumulative = 0;
            for (i, bound) in RATIO_BUCKETS.iter().enumerate() {
                cumulative += load(&c.ratio[i]);
                let _ = writeln!(
                    out,
                    "{prefix}_compression_ratio_bucket{{coding=\"{coding}\",le=\"{bound}\"}} {cumulative}"
                );
            }
            cumulative += load(&c.ratio[RATIO_BUCKETS.len()]);
            let _ = writeln!(
                out,
                "{prefix}_compression_ratio_bucket{{coding=\"{coding}\",le=\"+Inf\"}} {cumulative}"
            );
            let _ = writeln!(
                out,
                "{prefix}_compression_ratio_sum{{coding=\"{coding}\"}} {}",
                load(&c.ratio_sum_millis) as f64 / 1e3
            );
            let _ = writeln!(
                out,
                "{prefix}_compression_ratio_count{{coding=\"{coding}\"}} {cumulative}"
            );
        }
        fam(
            out,
            "compression_skipped_total",
            "Compressions skipped because the blocking pool was over its queue limit.",
            "counter",
        );
        let _ = writeln!(out, "{prefix}_compression_skipped_total {}", self.skipped());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn ae(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn norm(values: &[&str]) -> Option<Coding> {
        normalise(&ae(values))
    }

    #[test]
    fn browsers_headers_normalise_to_one_coding() {
        use Coding::*;
        assert_eq!(norm(&["gzip, deflate, br, zstd"]), Some(Br));
        assert_eq!(norm(&["gzip, deflate, br"]), Some(Br));
        assert_eq!(norm(&["gzip, deflate"]), Some(Gzip));
        assert_eq!(norm(&["br;q=1.0, gzip;q=0.5"]), Some(Br));
        assert_eq!(norm(&["gzip;q=1, br;q=0.9"]), Some(Gzip));
        assert_eq!(norm(&["zstd"]), Some(Zstd));
        assert_eq!(norm(&["X-GZIP"]), Some(Gzip));
        assert_eq!(norm(&["BR"]), Some(Br));
        // Two field lines are one list.
        assert_eq!(norm(&["gzip;q=0.5", "zstd;q=0.6"]), Some(Zstd));
        // No header: identity. An empty one: identity only.
        assert_eq!(normalise(&HeaderMap::new()), Some(Identity));
        assert_eq!(norm(&[""]), Some(Identity));
        assert_eq!(norm(&["deflate"]), Some(Identity));
    }

    #[test]
    fn weights_stars_and_identity() {
        use Coding::*;
        assert_eq!(norm(&["*"]), Some(Br));
        assert_eq!(norm(&["*;q=0.5, gzip"]), Some(Gzip));
        assert_eq!(norm(&["*;q=0"]), None);
        assert_eq!(norm(&["identity;q=0"]), None);
        assert_eq!(norm(&["gzip;q=0, identity;q=0"]), None);
        assert_eq!(norm(&["*;q=0, identity"]), Some(Identity));
        assert_eq!(norm(&["br;q=0, *"]), Some(Zstd));
        assert_eq!(norm(&["gzip;q=0.001"]), Some(Gzip));
        // Ties go br, zstd, gzip, identity.
        assert_eq!(norm(&["gzip, zstd"]), Some(Zstd));
        assert_eq!(norm(&["identity, gzip"]), Some(Gzip));
        // The first of a repeated coding counts.
        assert_eq!(norm(&["gzip;q=0, gzip"]), Some(Identity));
        let a = Accepted::from_headers(&ae(&["gzip;q=0.25, br;q=0"]));
        assert_eq!(
            (
                a.weight(Gzip),
                a.weight(Br),
                a.weight(Identity),
                a.weight(Zstd)
            ),
            (250, 0, 1, 0)
        );
        // Only what is available.
        assert_eq!(negotiate(&ae(&["br, gzip"]), &[Identity, Gzip]), Some(Gzip));
        assert_eq!(negotiate(&ae(&["br"]), &[Gzip]), None);
        assert_eq!(negotiate(&HeaderMap::new(), &[Gzip]), Some(Gzip));
    }

    #[test]
    fn malformed_members_are_skipped_not_the_field() {
        use Coding::*;
        for bad in [
            "gzip;q=2, br;q=0.5",
            "gzip;q=1.5, br;q=0.5",
            "gzip;q=0.5555, br;q=0.5",
            "gzip;q=, br;q=0.5",
            "gzip;q=abc, br;q=0.5",
            "gzip;level, br;q=0.5",
            "gzip;q=-1, br;q=0.5",
            "gzip;q=0.x, br;q=0.5",
        ] {
            assert_eq!(norm(&[bad]), Some(Br), "{bad}");
        }
        assert_eq!(norm(&[" , ,gzip , "]), Some(Gzip));
        assert_eq!(norm(&["gzip ; Q=0.5 ; foo=bar, br;q=0.4"]), Some(Gzip));
        let mut h = HeaderMap::new();
        h.append(
            header::ACCEPT_ENCODING,
            HeaderValue::from_bytes(b"\xffgzip").unwrap(),
        );
        h.append(header::ACCEPT_ENCODING, HeaderValue::from_static("zstd"));
        assert_eq!(normalise(&h), Some(Zstd));
        assert_eq!(parse_q("1.000"), Some(1000));
        assert_eq!(parse_q("0"), Some(0));
        assert_eq!(parse_q("1.001"), None);
        assert_eq!(Coding::Zstd.as_str(), "zstd");
        assert_eq!(Coding::Identity.as_str(), "identity");
    }

    #[test]
    fn compression_stats_add_up_and_render() {
        let stats = CompressionStats::default();
        assert_eq!(stats.micros_per_mib("br"), None);
        stats.record("br", 1 << 20, 100_000, Duration::from_millis(4));
        stats.record("br", 1 << 20, 900_000, Duration::from_millis(2));
        stats.record("identity", 10, 10, Duration::from_millis(1));
        stats.skip();
        assert_eq!(stats.bytes("br"), (2 << 20, 1_000_000));
        assert_eq!(stats.bytes("identity"), (0, 0));
        assert_eq!(stats.micros_per_mib("br"), Some(3000.0));
        assert_eq!(stats.skipped(), 1);
        let mut out = String::new();
        stats.render(&mut out, "cachet");
        for line in [
            "# TYPE cachet_compression_input_bytes_total counter",
            "cachet_compression_jobs_total{coding=\"br\"} 2",
            "cachet_compression_input_bytes_total{coding=\"br\"} 2097152",
            "cachet_compression_output_bytes_total{coding=\"br\"} 1000000",
            "cachet_compression_seconds_total{coding=\"br\"} 0.006",
            "cachet_compression_cpu_microseconds_per_mib{coding=\"br\"} 3000.0",
            "cachet_compression_cpu_microseconds_per_mib{coding=\"dcz\"} 0.0",
            // 0.095 and 0.858 of the source.
            "cachet_compression_ratio_bucket{coding=\"br\",le=\"0.1\"} 1",
            "cachet_compression_ratio_bucket{coding=\"br\",le=\"0.8\"} 1",
            "cachet_compression_ratio_bucket{coding=\"br\",le=\"1\"} 2",
            "cachet_compression_ratio_bucket{coding=\"br\",le=\"+Inf\"} 2",
            "cachet_compression_ratio_count{coding=\"br\"} 2",
            "cachet_compression_skipped_total 1",
        ] {
            assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
        }
        assert!(!out.contains("identity"));
    }
}
