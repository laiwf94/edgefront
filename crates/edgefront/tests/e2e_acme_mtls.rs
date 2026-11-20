//! End to end: `edgefront proxy` gets its certificate from Pebble over
//! ACME (TLS-ALPN-01 on its own listener), verifies a client's
//! certificate, and reaches a mutual-TLS upstream. Pebble runs in Docker;
//! no real CA is involved.
//!
//! The client trusts only Pebble's root and presents the fixture client
//! certificate; the proxy requires client certificates from the fixture
//! CA and sends the verified SPIFFE ID upstream; the upstream terminates
//! TLS with the fixture `api.example.com` certificate, requires the
//! proxy's own client certificate, and answers with who called it and on
//! whose behalf.

#[path = "../../edgefront-core/tests/common/pebble.rs"]
mod pebble;
#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::config::ClientAuth;
use edgefront_core::listener::{Handler, ServerLimits, serve_tls};
use edgefront_core::tls::{Acceptor, Snapshot, Store, TlsInfo};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream};

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");

struct Proc {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    fn wait_log(&self, needle: &str, within: Duration) -> String {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return line,
                Ok(line) => {
                    if line.contains("\"WARN\"") || line.contains("\"ERROR\"") {
                        eprintln!("{line}");
                    }
                }
                Err(_) => panic!("no log line with {needle}"),
            }
        }
    }
}

/// The upstream: says who called it (the proxy's client certificate) and
/// for whom (`X-Client-Spiffe-Id`).
struct Upstream;

impl Handler for Upstream {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        let caller = req
            .extensions()
            .get::<Arc<TlsInfo>>()
            .and_then(|t| t.client.as_ref())
            .and_then(|c| c.spiffe_id.clone())
            .unwrap_or_else(|| "anonymous".to_owned());
        let on_behalf = req
            .headers()
            .get("x-client-spiffe-id")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_else(|| "nobody".to_owned());
        http::Response::new(Full::new(Bytes::from(format!("{caller} for {on_behalf}"))))
    }
}

async fn mtls_upstream() -> SocketAddr {
    let mut tls = tlsc::tls_config(vec![tlsc::cert("exact", &[])]);
    tls.client_auth = Some(ClientAuth {
        ca_file: tlsc::fixture("ca.crt"),
        required: true,
    });
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(serve_tls(
        socket,
        Acceptor::new(Store::new(Arc::new(Snapshot::load(&tls).unwrap())), None),
        Arc::new(Upstream),
        ServerLimits::default(),
        std::future::pending(),
    ));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pebble_issues_and_a_client_certificate_reaches_an_mtls_upstream() {
    let port = pebble::free_port();
    let p = pebble::Pebble::start(pebble::free_port(), port, 90 * 86_400);
    let up = mtls_upstream().await;
    let dir = std::env::temp_dir().join(format!("e2e-acme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("acme");
    let config = serde_json::json!({
        "listen": format!("0.0.0.0:{port}"),
        "admin_listen": "127.0.0.1:0",
        "routes": [{"cluster": "web"}],
        "clusters": [{
            "name": "web",
            "endpoints": [{"address": up.to_string()}],
            "tls": {
                "ca_file": tlsc::fixture("ca.crt"),
                "server_name": "api.example.com",
                "cert_file": tlsc::fixture("client-rotated.crt"),
                "key_file": tlsc::fixture("client-rotated.key"),
            },
        }],
        "tls": {
            "certificates": [],
            "client_auth": {"ca_file": tlsc::fixture("ca.crt")},
            "acme": {
                "directory": p.directory,
                "contact": ["mailto:ops@edge.test"],
                "domains": ["edge.test"],
                "challenge": "tls-alpn-01",
                "state_dir": state.display().to_string(),
                "ca_file": p.api_ca.display().to_string(),
            },
        },
    });
    let path = dir.join("proxy.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let mut child = Command::new(EDGEFRONT)
        .args(["proxy", "--config"])
        .arg(&path)
        .env_remove("EDGEFRONT_ADMIN_TOKEN")
        .env("EDGEFRONT_DRAIN_PERIOD", "0s")
        .env("EDGEFRONT_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let proc = Proc { child, lines };
    let listening = proc.wait_log("\"listening\"", Duration::from_secs(10));
    let admin = {
        let key = "\"admin\":\"";
        let at = listening.find(key).unwrap() + key.len();
        listening[at..].split('"').next().unwrap().to_owned()
    };
    tokio::task::block_in_place(|| {
        proc.wait_log("acme certificate issued", Duration::from_secs(60))
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["account.key", "account.json", "cert.pem", "key.pem"] {
            let mode = std::fs::metadata(state.join(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{f}");
        }
    }

    // The client: trusts only Pebble's root, presents its certificate.
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(p.root_file()).unwrap())
        .unwrap();
    let client = |cert: Option<&str>| {
        let b = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots.clone());
        Arc::new(match cert {
            Some(c) => b
                .with_client_auth_cert(
                    vec![
                        CertificateDer::from_pem_file(tlsc::fixture(&format!("{c}.crt"))).unwrap(),
                    ],
                    PrivateKeyDer::from_pem_file(tlsc::fixture(&format!("{c}.key"))).unwrap(),
                )
                .unwrap(),
            None => b.with_no_client_auth(),
        })
    };
    let request = |cfg: Arc<rustls::ClientConfig>| async move {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let tls = tokio_rustls::TlsConnector::from(cfg)
            .connect(ServerName::try_from("edge.test").unwrap(), tcp)
            .await
            .map_err(|e| e.to_string())?;
        let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let req = http::Request::get("/")
            .header("host", "edge.test")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = send.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Ok::<_, String>((status, String::from_utf8_lossy(&body).into_owned()))
    };
    assert_eq!(
        request(client(Some("client"))).await.unwrap(),
        (
            200,
            "spiffe://edge.test/ns/web/sa/rotated for spiffe://edge.test/ns/web/sa/frontend"
                .to_owned()
        )
    );
    // Without a client certificate the edge refuses the handshake.
    assert!(request(client(None)).await.is_err());

    // The renewal series are on /metrics.
    let metrics = {
        use std::io::{Read, Write};
        let mut c = std::net::TcpStream::connect(&admin).unwrap();
        c.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    };
    assert!(
        metrics.contains("edgefront_acme_renewals_total{result=\"success\"} 1"),
        "{metrics}"
    );
    let expiry: f64 = metrics
        .lines()
        .find_map(|l| l.strip_prefix("edgefront_acme_certificate_expiry_seconds "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(expiry > 89.0 * 86_400.0, "{expiry}");
    drop(proc);
    let _ = std::fs::remove_dir_all(&dir);
}
