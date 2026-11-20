//! A 2 GiB object end to end through the real binary: cached as 1 MiB
//! slices on the segment store, the node killed with SIGKILL halfway
//! through the fill, restarted, and asked for ranges, every byte of which
//! is checked.
mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{Binary, Scratch, binary_config, http};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const LEN: u64 = 2 << 30;
const CHUNK: usize = 256 << 10;

fn byte(i: u64) -> u8 {
    (i.wrapping_mul(2_654_435_761) >> 13) as u8
}

fn expected(start: u64, end: u64) -> Vec<u8> {
    (start..=end).map(byte).collect()
}

/// An origin serving `/large`, 2 GiB generated as it is sent, whole or one
/// range; it counts the requests for it.
async fn origin() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fetches = Arc::new(AtomicUsize::new(0));
    let count = fetches.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut request = String::new();
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if request.is_empty() {
                            request = line.clone();
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            range = Some(v.trim().to_owned());
                        }
                    }
                    if !request.contains("/large") {
                        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                        if w.write_all(ok).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let (status, start, end) = match range.as_deref() {
                        Some(spec) => {
                            let (a, b) = spec.split_once('-').unwrap();
                            let b = b.parse::<u64>().unwrap_or(LEN - 1).min(LEN - 1);
                            ("206 Partial Content", a.parse::<u64>().unwrap(), b)
                        }
                        None => ("200 OK", 0, LEN - 1),
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status}\r\ncache-control: max-age=3600\r\netag: \"big-1\"\r\ncontent-length: {}\r\n",
                        end - start + 1
                    );
                    if status.starts_with("206") {
                        head.push_str(&format!("content-range: bytes {start}-{end}/{LEN}\r\n"));
                    }
                    head.push_str("\r\n");
                    if w.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut at = start;
                    let mut buf = vec![0u8; CHUNK];
                    while at <= end {
                        let n = (end - at + 1).min(CHUNK as u64) as usize;
                        for (k, b) in buf[..n].iter_mut().enumerate() {
                            *b = byte(at + k as u64);
                        }
                        if w.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                        at += n as u64;
                    }
                }
            });
        }
    });
    (addr, fetches)
}

/// Reads the head of a response on `conn`.
async fn read_head(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            return head;
        }
        head.push_str(&line.to_ascii_lowercase());
    }
}

/// `GET /large` for bytes `start..=end`, checking every byte and the
/// status; returns the response head.
async fn get_range(addr: SocketAddr, start: u64, end: u64) -> String {
    let conn = TcpStream::connect(addr).await.unwrap();
    let (r, mut w) = conn.into_split();
    let req = format!(
        "GET /large HTTP/1.1\r\nHost: big.test\r\nRange: bytes={start}-{end}\r\nConnection: close\r\n\r\n"
    );
    w.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(r);
    let head = read_head(&mut r).await;
    assert!(head.starts_with("http/1.1 206"), "{head}");
    assert!(
        head.contains(&format!("content-range: bytes {start}-{end}/{LEN}")),
        "{head}"
    );
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(300), r.read_to_end(&mut body))
        .await
        .expect("the range took over five minutes")
        .unwrap();
    assert_eq!(body.len() as u64, end - start + 1, "{head}");
    assert!(body == expected(start, end), "wrong bytes in {start}-{end}");
    head
}

fn segment_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir.join("segments"))
        .map(|d| {
            d.filter_map(|e| e.ok()?.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_2_gib_object_survives_a_kill_mid_fill_and_serves_ranges() {
    let (origin, fetches) = origin().await;
    let dir = Scratch::new("large");
    let disk = dir.0.join("cache");
    let extra = format!(
        "[cache]\nmemory_bytes = 268435456\nslice_bytes = 1048576\n\
         [cache.disk]\npath = \"{}\"\nmax_bytes = 3221225472\nqueue = 4096\n\
         [timeouts]\nbody_ms = 900000\nresponse_ms = 60000\n",
        disk.display()
    );
    let config = binary_config("edge-big", origin, &extra);

    // A whole fetch, cut off by SIGKILL after about 600 MiB.
    let bin = Binary::start(&dir.0, &config, &[]);
    let conn = TcpStream::connect(bin.proxy_addr).await.unwrap();
    let (r, mut w) = conn.into_split();
    w.write_all(b"GET /large HTTP/1.1\r\nHost: big.test\r\n\r\n")
        .await
        .unwrap();
    let mut r = BufReader::new(r);
    let head = read_head(&mut r).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains(&format!("content-length: {LEN}")), "{head}");
    let mut seen = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    while seen < 600 << 20 {
        let n = r.read(&mut buf).await.unwrap();
        assert!(n > 0, "the body ended at {seen}");
        assert!(
            buf[..n] == expected(seen, seen + n as u64 - 1)[..],
            "wrong bytes at {seen}"
        );
        seen += n as u64;
    }
    // Let some of the slices reach the disk, then kill the node mid-fill.
    common::eventually("slices on disk", || segment_bytes(&disk) > 100 << 20).await;
    bin.signal("KILL");
    drop(bin);
    drop(r);
    let torn = segment_bytes(&disk);
    assert!(
        torn > 0 && torn < LEN,
        "{torn} bytes of segments after the kill"
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 1);

    // Restart. The fill never finished, so no manifest: the first range
    // fetches the object once, and is answered as soon as its slice is in.
    let bin = Binary::start(&dir.0, &config, &[]);
    let started = Instant::now();
    get_range(bin.proxy_addr, 100_000_000, 100_000_999).await;
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    // Once the 2 GiB are in, ranges anywhere are hits from the slices.
    let mut hit = String::new();
    for _ in 0..600 {
        hit = get_range(bin.proxy_addr, LEN - 1000, LEN - 1).await;
        if hit.contains("edge-big; hit") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(hit.contains("edge-big; hit"), "{hit}");
    let fills = fetches.load(Ordering::SeqCst);
    for (start, end) in [
        (0, 999),
        ((1 << 20) - 500, (1 << 20) + 499),
        (1 << 30, (1 << 30) + (3 << 20)),
        (LEN / 3, LEN / 3 + 12_345),
        (LEN - 1, LEN - 1),
    ] {
        let head = get_range(bin.proxy_addr, start, end).await;
        assert!(head.contains("edge-big; hit"), "{start}-{end}: {head}");
    }
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        fills,
        "a range hit went to the origin"
    );
    // The slices reach the segment store behind the memory tier: all
    // 2,048 of them and the manifest, once the write queue drains.
    let mut objects = 0u64;
    let mut text = String::new();
    for _ in 0..600 {
        let metrics = http("GET", bin.admin_addr, "/metrics", &[], "").await;
        text = String::from_utf8_lossy(&metrics.body).into_owned();
        objects = text
            .lines()
            .find_map(|l| l.strip_prefix("cachet_disk_objects "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if objects > 2048 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let disk_lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("cachet_disk"))
        .collect();
    assert!(objects > 2048, "{objects} objects on disk: {disk_lines:#?}");
}
