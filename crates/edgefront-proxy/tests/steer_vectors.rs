//! Request paths and forwarding headers against steer itself.
//!
//! The fixtures were produced by running steer's own code under Go 1.27.3
//! (`url.ParseRequestURI`, `route.CleanPath` and the rewrite in
//! `internal/proxy/proxy.go` for paths; `ClientIP` and `setForwarded` for
//! headers) over hand-picked and generated inputs. Every value is written
//! with Go's `strconv.QuoteToASCII`, so arbitrary bytes survive.

use std::net::IpAddr;

use edgefront_core::config::IpPrefix;
use edgefront_proxy::forwarded::{
    X_FORWARDED_FOR, X_FORWARDED_HOST, X_FORWARDED_PROTO, client_ip, peer_addr, set_forwarded,
};
use edgefront_proxy::path::RequestTarget;
use http::header::{FORWARDED, HeaderMap, HeaderName, HeaderValue};

/// Reverses Go's `strconv.QuoteToASCII`.
fn unquote(q: &str) -> Vec<u8> {
    let b = q.as_bytes();
    assert!(
        b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"',
        "not quoted: {q}"
    );
    let body = &q[1..q.len() - 1];
    let mut out = Vec::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        fn hex(chars: &mut std::str::Chars<'_>, n: usize) -> u32 {
            let digits: String = chars.take(n).collect();
            u32::from_str_radix(&digits, 16).unwrap()
        }
        match chars.next().unwrap() {
            'a' => out.push(7),
            'b' => out.push(8),
            'f' => out.push(12),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'v' => out.push(11),
            '\\' => out.push(b'\\'),
            '"' => out.push(b'"'),
            'x' => out.push(u8::try_from(hex(&mut chars, 2)).unwrap()),
            'u' => {
                let c = char::from_u32(hex(&mut chars, 4)).unwrap();
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            'U' => {
                let c = char::from_u32(hex(&mut chars, 8)).unwrap();
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            other => panic!("unknown escape \\{other} in {q}"),
        }
    }
    out
}

fn lines(fixture: &str) -> impl Iterator<Item = Vec<&str>> {
    fixture
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| l.split('\t').collect())
}

#[test]
fn unquote_handles_every_go_escape() {
    assert_eq!(
        unquote(r#""\a\b\f\n\r\t\v\\\"\x00\xffé\U0001f600x""#),
        b"\x07\x08\x0c\n\r\t\x0b\\\"\x00\xff\xc3\xa9\xf0\x9f\x98\x80x"
    );
}

#[test]
fn paths_match_steer() {
    let fixture = include_str!("fixtures/steer_paths.txt");
    let (mut accepted, mut rejected, mut cleaned) = (0, 0, 0);
    for fields in lines(fixture) {
        let target = unquote(fields[0]);
        let shown = String::from_utf8_lossy(&target).into_owned();
        let parsed = RequestTarget::parse(&target);
        if fields[1] == "error" {
            assert!(parsed.is_err(), "{shown:?}: steer rejects, got {parsed:?}");
            rejected += 1;
            continue;
        }
        let t = parsed.unwrap_or_else(|e| panic!("{shown:?}: steer accepts, got {e}"));
        assert_eq!(t.path(), unquote(fields[1]), "{shown:?}: decoded path");
        assert_eq!(
            t.clean_path(),
            unquote(fields[2]),
            "{shown:?}: cleaned path"
        );
        assert_eq!(
            &*t.upstream_target(),
            unquote(fields[3]),
            "{shown:?}: upstream target"
        );
        accepted += 1;
        cleaned += usize::from(t.was_cleaned());
    }
    assert_eq!(accepted + rejected, 1500);
    assert!(
        rejected > 100 && accepted > 900 && cleaned > 150,
        "{accepted} {rejected} {cleaned}"
    );
}

#[test]
fn forwarded_headers_match_steer() {
    let trusted: Vec<IpPrefix> = ["10.0.0.0/8", "::1/128", "127.0.0.0/8"]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    let fixture = include_str!("fixtures/steer_forwarded.txt");
    let (mut checked, mut unrepresentable) = (0, 0);
    'case: for f in lines(fixture) {
        let remote = String::from_utf8(unquote(f[0])).unwrap();
        let tls = f[1] == "1";
        let host = unquote(f[2]);
        let xff: Vec<Vec<u8>> = match f[3] {
            "" => Vec::new(),
            list => list.split('|').map(unquote).collect(),
        };
        let (proto, xfh) = (unquote(f[4]), unquote(f[5]));
        let want_client = String::from_utf8(unquote(f[6])).unwrap();
        let want = |i: usize| (f[i] != "-").then(|| unquote(f[i]));

        let mut headers = HeaderMap::new();
        let mut add = |name: &HeaderName, value: &[u8]| match HeaderValue::from_bytes(value) {
            Ok(v) => {
                headers.append(name, v);
                true
            }
            Err(_) => false,
        };
        let mut ok = add(&FORWARDED, b"for=9.9.9.9");
        for v in &xff {
            ok &= add(&X_FORWARDED_FOR, v);
        }
        if !proto.is_empty() {
            ok &= add(&X_FORWARDED_PROTO, &proto);
        }
        if !xfh.is_empty() {
            ok &= add(&X_FORWARDED_HOST, &xfh);
        }
        if !ok {
            // A value hyper would refuse (DEL): no request could carry it.
            unrepresentable += 1;
            continue 'case;
        }

        let peer = peer_addr(&remote);
        let ctx = format!(
            "{remote:?} tls={tls} host={:?} xff={xff:?}",
            String::from_utf8_lossy(&host)
        );
        let client = client_ip(peer, xff.iter().map(Vec::as_slice), &trusted);
        let show = |a: Option<IpAddr>| a.map(|a| a.to_string()).unwrap_or_default();
        assert_eq!(show(client), want_client, "{ctx}: ClientIP");

        let returned = set_forwarded(&mut headers, peer, tls, &host, &trusted);
        assert_eq!(returned, client, "{ctx}: set_forwarded's client");
        assert!(headers.get(&FORWARDED).is_none(), "{ctx}: Forwarded kept");
        for (i, name) in [
            (7, &X_FORWARDED_FOR),
            (8, &X_FORWARDED_PROTO),
            (9, &X_FORWARDED_HOST),
        ] {
            let got: Vec<&[u8]> = headers
                .get_all(name)
                .iter()
                .map(HeaderValue::as_bytes)
                .collect();
            match want(i) {
                Some(w) => assert_eq!(got, [w.as_slice()], "{ctx}: {name}"),
                None => assert!(got.is_empty(), "{ctx}: {name} should be unset, got {got:?}"),
            }
        }
        checked += 1;
    }
    assert!(checked > 1000, "only {checked} cases checked");
    assert!(unrepresentable < 150, "{unrepresentable} cases skipped");
}
