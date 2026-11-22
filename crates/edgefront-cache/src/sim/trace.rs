//! Streaming trace readers.
//!
//! Two formats, read one record at a time through a buffer so a trace of
//! any length replays in constant memory:
//!
//! - **oracleGeneral**, libCacheSim's binary format, in which the public CDN
//!   and key-value traces are published: 24-byte little-endian records of a
//!   u32 timestamp, a u64 object id, a u32 object size and an i64 next
//!   access (ignored here).
//! - **CSV**, with the delimiter and the 1-based columns of the id, size and
//!   timestamp given (default `time,id,size`). A numeric id is used as it
//!   is and any other is hashed with 64-bit FNV-1a; blank lines and lines
//!   starting with `#` are skipped, and a first line whose numeric columns
//!   do not parse is taken as a header. Any other malformed line is an
//!   error naming its line number.
//!
//! A file ending `.zst` or `.gz` is decompressed as it is read (with the
//! `compression` feature), and `-` reads standard input.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use super::Request;

/// The size of an oracleGeneral record.
pub const ORACLE_RECORD: usize = 24;

/// Read buffer for traces: large reads, few system calls.
const BUFFER: usize = 1 << 20;

/// A boxed trace, as [`open`] returns it.
pub type Trace = Box<dyn Iterator<Item = io::Result<Request>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    OracleGeneral,
    Csv(CsvColumns),
}

impl Format {
    /// The format a file name suggests: CSV for names containing `.csv`
    /// (as in `trace.csv.zst`), oracleGeneral otherwise.
    pub fn guess(path: &Path) -> Format {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase());
        match name {
            Some(n) if n.contains(".csv") => Format::Csv(CsvColumns::default()),
            _ => Format::OracleGeneral,
        }
    }
}

/// Where a CSV trace keeps its fields. Columns are 0-based here and 1-based
/// on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvColumns {
    pub delimiter: u8,
    pub time: Option<usize>,
    pub id: usize,
    /// Without a size column every object is 1 byte.
    pub size: Option<usize>,
    /// Skip the first line whatever it holds.
    pub header: bool,
}

impl Default for CsvColumns {
    fn default() -> Self {
        CsvColumns {
            delimiter: b',',
            time: Some(0),
            id: 1,
            size: Some(2),
            header: false,
        }
    }
}

impl CsvColumns {
    /// Parses `time=1,id=2,size=3`: 1-based columns by name, any subset
    /// naming `id`. A column of 0 or `-` leaves that field out.
    pub fn parse_spec(spec: &str, delimiter: u8, header: bool) -> Result<CsvColumns, String> {
        let mut out = CsvColumns {
            delimiter,
            time: None,
            id: usize::MAX,
            size: None,
            header,
        };
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (name, col) = part
                .split_once('=')
                .ok_or_else(|| format!("column `{part}` is not name=number"))?;
            let col = match col.trim() {
                "-" => None,
                c => c
                    .parse::<usize>()
                    .map_err(|_| format!("column `{part}` is not name=number"))?
                    .checked_sub(1),
            };
            match name.trim() {
                "time" => out.time = col,
                "id" => out.id = col.ok_or("the id column cannot be left out")?,
                "size" => out.size = col,
                other => return Err(format!("unknown column `{other}` (time, id or size)")),
            }
        }
        if out.id == usize::MAX {
            return Err("the columns must name id".into());
        }
        Ok(out)
    }
}

/// Opens a trace file, or standard input for `-`, decompressing by
/// extension.
pub fn open(path: &Path, format: &Format) -> io::Result<Trace> {
    let raw: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(path)?)
    };
    let name = path.to_string_lossy().to_ascii_lowercase();
    let input = decompress(raw, &name)?;
    let input = BufReader::with_capacity(BUFFER, input);
    Ok(match format {
        Format::OracleGeneral => Box::new(OracleReader::new(input)),
        Format::Csv(cols) => Box::new(CsvReader::new(input, cols.clone())),
    })
}

#[cfg(feature = "compression")]
fn decompress(raw: Box<dyn Read>, name: &str) -> io::Result<Box<dyn Read>> {
    if name.ends_with(".zst") || name.ends_with(".zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(raw)?;
        // Traces are often compressed with --long=31.
        decoder.window_log_max(31)?;
        Ok(Box::new(decoder))
    } else if name.ends_with(".gz") {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(raw)))
    } else {
        Ok(raw)
    }
}

#[cfg(not(feature = "compression"))]
fn decompress(raw: Box<dyn Read>, name: &str) -> io::Result<Box<dyn Read>> {
    if name.ends_with(".zst") || name.ends_with(".zstd") || name.ends_with(".gz") {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "compressed traces need edgefront-cache's compression feature",
        ))
    } else {
        Ok(raw)
    }
}

/// libCacheSim's oracleGeneral records.
pub struct OracleReader<R> {
    input: R,
    record: u64,
    failed: bool,
}

impl<R: Read> OracleReader<R> {
    pub fn new(input: R) -> Self {
        OracleReader {
            input,
            record: 0,
            failed: false,
        }
    }

    fn read_record(&mut self) -> io::Result<Option<Request>> {
        let mut buf = [0u8; ORACLE_RECORD];
        let mut filled = 0;
        while filled < ORACLE_RECORD {
            match self.input.read(&mut buf[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "record {} is truncated: {filled} of {ORACLE_RECORD} bytes",
                            self.record + 1
                        ),
                    ));
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.record += 1;
        let time = u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes"));
        let id = u64::from_le_bytes(buf[4..12].try_into().expect("8 bytes"));
        let size = u32::from_le_bytes(buf[12..16].try_into().expect("4 bytes"));
        Ok(Some(Request {
            time: u64::from(time),
            id,
            size: u64::from(size).max(1),
        }))
    }
}

impl<R: Read> Iterator for OracleReader<R> {
    type Item = io::Result<Request>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let out = self.read_record().transpose();
        self.failed = matches!(out, Some(Err(_)));
        out
    }
}

/// Encodes requests as oracleGeneral records, for tests and generators.
pub fn oracle_record(req: &Request) -> [u8; ORACLE_RECORD] {
    let mut out = [0u8; ORACLE_RECORD];
    out[0..4].copy_from_slice(&(req.time as u32).to_le_bytes());
    out[4..12].copy_from_slice(&req.id.to_le_bytes());
    out[12..16].copy_from_slice(&(req.size as u32).to_le_bytes());
    out[16..24].copy_from_slice(&(-1i64).to_le_bytes());
    out
}

/// CSV traces, one request a line.
pub struct CsvReader<R> {
    input: R,
    cols: CsvColumns,
    line: Vec<u8>,
    line_no: u64,
    /// Data lines seen, for the header rule.
    data_lines: u64,
    failed: bool,
}

impl<R: BufRead> CsvReader<R> {
    pub fn new(input: R, cols: CsvColumns) -> Self {
        CsvReader {
            input,
            cols,
            line: Vec::new(),
            line_no: 0,
            data_lines: 0,
            failed: false,
        }
    }

    fn read_request(&mut self) -> io::Result<Option<Request>> {
        loop {
            self.line.clear();
            if self.input.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(None);
            }
            self.line_no += 1;
            let line = trim(&self.line);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            self.data_lines += 1;
            if self.data_lines == 1 && self.cols.header {
                continue;
            }
            match parse_line(line, &self.cols) {
                Ok(mut req) => {
                    if self.cols.time.is_none() {
                        req.time = self.line_no;
                    }
                    return Ok(Some(req));
                }
                // A first line whose numbers do not parse is a header.
                Err(_) if self.data_lines == 1 && !self.cols.header => continue,
                Err(why) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("line {}: {why}", self.line_no),
                    ));
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for CsvReader<R> {
    type Item = io::Result<Request>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let out = self.read_request().transpose();
        self.failed = matches!(out, Some(Err(_)));
        out
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn field(s: &[u8]) -> &[u8] {
    let s = trim(s);
    match s {
        [b'"', inner @ .., b'"'] => inner,
        _ => s,
    }
}

fn parse_line(line: &[u8], cols: &CsvColumns) -> Result<Request, String> {
    let mut id = None;
    let mut size = None;
    let mut time = None;
    let wanted = cols
        .id
        .max(cols.size.unwrap_or(0))
        .max(cols.time.unwrap_or(0));
    for (i, f) in line.split(|&b| b == cols.delimiter).enumerate() {
        if i == cols.id {
            id = Some(field(f));
        }
        if Some(i) == cols.size {
            size = Some(field(f));
        }
        if Some(i) == cols.time {
            time = Some(field(f));
        }
        if i >= wanted {
            break;
        }
    }
    let missing = |name: &str, col: usize| format!("no {name} in column {}", col + 1);
    let id = id.ok_or_else(|| missing("id", cols.id))?;
    if id.is_empty() {
        return Err("empty id".into());
    }
    let id = number(id).unwrap_or_else(|| fnv1a(id));
    let size = match cols.size {
        None => 1,
        Some(c) => {
            let s = size.ok_or_else(|| missing("size", c))?;
            number(s).ok_or_else(|| format!("size `{}` is not a whole number", show(s)))?
        }
    };
    let time = match cols.time {
        None => 0,
        Some(c) => {
            let t = time.ok_or_else(|| missing("time", c))?;
            timestamp(t).ok_or_else(|| format!("time `{}` is not a number", show(t)))?
        }
    };
    Ok(Request {
        time,
        id,
        size: size.max(1),
    })
}

fn number(s: &[u8]) -> Option<u64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Whole or fractional timestamps; a fraction is dropped.
fn timestamp(s: &[u8]) -> Option<u64> {
    if let Some(n) = number(s) {
        return Some(n);
    }
    let f: f64 = std::str::from_utf8(s).ok()?.parse().ok()?;
    (f.is_finite() && f >= 0.0).then_some(f as u64)
}

fn show(s: &[u8]) -> String {
    String::from_utf8_lossy(&s[..s.len().min(40)]).into_owned()
}

/// 64-bit FNV-1a, a stable hash for ids that are not numbers.
pub fn fnv1a(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(time: u64, id: u64, size: u64) -> Request {
        Request { time, id, size }
    }

    fn csv(text: &str, cols: CsvColumns) -> Vec<io::Result<Request>> {
        CsvReader::new(text.as_bytes(), cols).collect()
    }

    fn ok(v: Vec<io::Result<Request>>) -> Vec<Request> {
        v.into_iter().map(Result::unwrap).collect()
    }

    /// Hands out one byte per read, to exercise records split across reads.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.split_first() {
                Some((&b, rest)) if !buf.is_empty() => {
                    buf[0] = b;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn oracle_general_round_trip() {
        let reqs = [
            req(1, 42, 1_000),
            req(2, u64::MAX, u64::from(u32::MAX)),
            req(3, 0, 7),
        ];
        let bytes: Vec<u8> = reqs.iter().flat_map(oracle_record).collect();
        assert_eq!(bytes.len(), 3 * ORACLE_RECORD);
        let got: Vec<Request> = OracleReader::new(&bytes[..]).map(Result::unwrap).collect();
        assert_eq!(got, reqs);
        let got: Vec<Request> = OracleReader::new(Trickle(&bytes))
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, reqs);
    }

    #[test]
    fn oracle_general_sizes_of_zero_count_as_one_byte() {
        let bytes = oracle_record(&req(5, 9, 0));
        let got: Vec<Request> = OracleReader::new(&bytes[..]).map(Result::unwrap).collect();
        assert_eq!(got, [req(5, 9, 1)]);
    }

    #[test]
    fn oracle_general_truncated_record_is_an_error_then_the_end() {
        let mut bytes = oracle_record(&req(1, 1, 1)).to_vec();
        bytes.extend_from_slice(&oracle_record(&req(2, 2, 2))[..10]);
        let mut r = OracleReader::new(&bytes[..]);
        assert_eq!(r.next().unwrap().unwrap(), req(1, 1, 1));
        let err = r.next().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(err.to_string(), "record 2 is truncated: 10 of 24 bytes");
        assert!(r.next().is_none());
    }

    #[test]
    fn oracle_general_passes_read_errors_on() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    self.0 = false;
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Err(io::Error::other("disk gone"))
                }
            }
        }
        let mut r = OracleReader::new(Failing(true));
        assert_eq!(r.next().unwrap().unwrap_err().to_string(), "disk gone");
        assert!(r.next().is_none());
    }

    #[test]
    fn csv_default_columns_with_a_header_comments_and_blank_lines() {
        let text =
            "timestamp,obj_id,obj_size\n# a comment\n\n10,1,100\n 11 , 2 , 200 \r\n12,\"3\",300";
        assert_eq!(
            ok(csv(text, CsvColumns::default())),
            [req(10, 1, 100), req(11, 2, 200), req(12, 3, 300)]
        );
    }

    #[test]
    fn csv_string_ids_are_hashed_stably() {
        let text = "1,/img/a.png,10\n2,/img/b.png,20\n3,/img/a.png,10\n";
        let got = ok(csv(text, CsvColumns::default()));
        assert_eq!(got[0].id, got[2].id);
        assert_ne!(got[0].id, got[1].id);
        assert_eq!(got[0].id, fnv1a(b"/img/a.png"));
        // FNV-1a's published value for "a".
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn csv_columns_by_spec() {
        let cols = CsvColumns::parse_spec("id=3, size=1", b'\t', true).unwrap();
        assert_eq!(
            cols,
            CsvColumns {
                delimiter: b'\t',
                time: None,
                id: 2,
                size: Some(0),
                header: true,
            }
        );
        let text = "size\tx\tid\n500\tx\t7\n600\tx\t8\n";
        // Without a time column, the time is the line number.
        assert_eq!(ok(csv(text, cols)), [req(2, 7, 500), req(3, 8, 600)]);

        let ids_only = CsvColumns::parse_spec("id=1,size=-,time=0", b',', false).unwrap();
        assert_eq!(ok(csv("5\n6\n", ids_only)), [req(1, 5, 1), req(2, 6, 1)]);

        for bad in [
            "",
            "size=2",
            "id",
            "id=x",
            "id=0",
            "id=00",
            "id=-",
            "colour=1",
            "id=1,size=y",
        ] {
            assert!(CsvColumns::parse_spec(bad, b',', false).is_err(), "{bad}");
        }
    }

    #[test]
    fn csv_fractional_times_and_zero_sizes() {
        let got = ok(csv("1.75,1,0\n2.5,2,3\n", CsvColumns::default()));
        assert_eq!(got, [req(1, 1, 1), req(2, 2, 3)]);
    }

    #[test]
    fn csv_malformed_lines_name_their_line() {
        let cases = [
            (
                "1,1,10\n2,2,ten\n",
                "line 2: size `ten` is not a whole number",
            ),
            ("1,1,10\n2,2\n", "line 2: no size in column 3"),
            ("1,1,10\nnow,2,10\n", "line 2: time `now` is not a number"),
            ("1,1,10\n-1,2,10\n", "line 2: time `-1` is not a number"),
            ("1,1,10\n3,,10\n", "line 2: empty id"),
            ("1,1,10\n3\n", "line 2: no id in column 2"),
        ];
        for (text, want) in cases {
            let got = csv(text, CsvColumns::default());
            assert_eq!(got.len(), 2, "{text:?}");
            assert_eq!(got[1].as_ref().unwrap_err().to_string(), want);
        }
        // Only the first data line may be a header; after it, nothing is read.
        let mut r = CsvReader::new("a,b,c\nd,e,f\n1,1,1\n".as_bytes(), CsvColumns::default());
        let err = r.next().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(r.next().is_none());
    }

    #[test]
    fn format_from_the_file_name() {
        assert!(matches!(
            Format::guess(Path::new("traces/cdn.CSV.zst")),
            Format::Csv(_)
        ));
        assert_eq!(
            Format::guess(Path::new("wiki.oracleGeneral.zst")),
            Format::OracleGeneral
        );
        assert_eq!(Format::guess(Path::new("/")), Format::OracleGeneral);
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("edgefront-sim-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn opens_plain_files() {
        let path = temp("plain.oracleGeneral.bin");
        let reqs = [req(1, 2, 3), req(4, 5, 6)];
        let bytes: Vec<u8> = reqs.iter().flat_map(oracle_record).collect();
        std::fs::write(&path, &bytes).unwrap();
        let got: Vec<Request> = open(&path, &Format::OracleGeneral)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, reqs);
        let path = temp("plain.csv");
        std::fs::write(&path, "1,2,3\n4,5,6\n").unwrap();
        let got: Vec<Request> = open(&path, &Format::guess(&path))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, reqs);
        assert!(open(&temp("missing.csv"), &Format::OracleGeneral).is_err());
    }

    #[cfg(feature = "compression")]
    #[test]
    fn opens_zstd_and_gzip_files() {
        use std::io::Write;
        let reqs: Vec<Request> = (0..5_000).map(|i| req(i, i % 97, 1 + i % 13)).collect();
        let bytes: Vec<u8> = reqs.iter().flat_map(oracle_record).collect();

        let path = temp("t.oracleGeneral.zst");
        std::fs::write(&path, zstd::encode_all(&bytes[..], 19).unwrap()).unwrap();
        let got: Vec<Request> = open(&path, &Format::OracleGeneral)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, reqs);

        let path = temp("t.oracleGeneral.gz");
        // Two gzip members, as concatenated files are.
        let mut out = Vec::new();
        for half in bytes.chunks(bytes.len() / 2) {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(half).unwrap();
            out.extend(gz.finish().unwrap());
        }
        std::fs::write(&path, out).unwrap();
        let got: Vec<Request> = open(&path, &Format::OracleGeneral)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, reqs);

        let path = temp("broken.zst");
        std::fs::write(&path, b"not zstd at all").unwrap();
        let first = open(&path, &Format::OracleGeneral).unwrap().next().unwrap();
        assert!(first.is_err());
    }
}
