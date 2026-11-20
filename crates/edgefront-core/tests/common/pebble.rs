//! Pebble, Let's Encrypt's ACME test server, in Docker, for the ACME tests:
//! never a real CA.
//!
//! [`Pebble::start`] runs `pebble-challtestsrv` as Pebble's DNS server and
//! Pebble itself on a private network. challtestsrv answers every A query
//! with the host's address as the containers see it, so Pebble validates
//! HTTP-01 and TLS-ALPN-01 against listeners the test opens on the host,
//! on the ports given to `start` (Pebble's `httpPort` and `tlsPort`), and
//! DNS-01 against TXT records set through challtestsrv's management API.
//! Pebble's own API certificate is its test minica, copied out of the
//! image, and the root it signs certificates with comes from its
//! management API. Both containers and the network are removed when the
//! value drops.
//!
//! The images are pinned by digest. Pebble runs with
//! `PEBBLE_VA_NOSLEEP=1` (no random validation delay) and
//! `PEBBLE_WFE_NONCEREJECT=0` (no random `badNonce`), so a run's timing
//! is the test's own.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const PEBBLE: &str = "ghcr.io/letsencrypt/pebble@sha256:0c81f05db371f436c97fb894bd6d2ab8fc9c8e2ce40f8bef8a8e15941bcd75ef";
pub const CHALLTESTSRV: &str = "ghcr.io/letsencrypt/pebble-challtestsrv@sha256:0152c8cb506e2ecba2900ed47d23932e27e3e6575a7d2c8ef93c7719ee3dea4c";
pub const ALPINE: &str = "alpine:3.20";

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("docker {args:?}: {e}; the ACME tests need Docker"));
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The host's address as containers on a user network reach it.
pub fn host_ip() -> String {
    let out = docker(&[
        "run",
        "--rm",
        "--add-host=host.docker.internal:host-gateway",
        ALPINE,
        "getent",
        "hosts",
        "host.docker.internal",
    ]);
    out.split_whitespace()
        .next()
        .expect("host.docker.internal resolves")
        .to_owned()
}

/// A running Pebble.
pub struct Pebble {
    names: Vec<String>,
    network: String,
    /// The ACME directory URL.
    pub directory: String,
    /// Pebble's API CA (its minica), to trust for `directory`.
    pub api_ca: PathBuf,
    /// The management API, `https://localhost:<port>` (Pebble's API certificate).
    pub management: String,
    /// challtestsrv's management API, `http://127.0.0.1:<port>`.
    pub dns_management: String,
    dir: PathBuf,
}

impl Drop for Pebble {
    fn drop(&mut self) {
        for n in &self.names {
            let _ = Command::new("docker").args(["rm", "-f", n]).output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static SEQ: AtomicU32 = AtomicU32::new(0);

fn published(name: &str, port: u16) -> u16 {
    let out = docker(&["port", name, &format!("{port}/tcp")]);
    let first = out.lines().next().expect("a published port");
    first.rsplit(':').next().unwrap().parse().unwrap()
}

impl Pebble {
    /// Starts Pebble validating HTTP-01 on the host's `http_port` and
    /// TLS-ALPN-01 on its `tls_port`, with certificates valid for
    /// `validity_secs`.
    pub fn start(http_port: u16, tls_port: u16, validity_secs: u64) -> Pebble {
        let tag = format!(
            "efacme-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let dir = std::env::temp_dir().join(&tag);
        std::fs::create_dir_all(&dir).unwrap();
        let network = format!("{tag}-net");
        docker(&["network", "create", &network]);
        let ip = host_ip();
        let dns = format!("{tag}-dns");
        let pebble = format!("{tag}-pebble");
        let mut p = Pebble {
            names: Vec::new(),
            network: network.clone(),
            directory: String::new(),
            api_ca: dir.join("pebble.minica.pem"),
            management: String::new(),
            dns_management: String::new(),
            dir: dir.clone(),
        };
        docker(&[
            "run",
            "-d",
            "--name",
            &dns,
            "--network",
            &network,
            "--network-alias",
            "challtestsrv",
            "-p",
            "127.0.0.1::8055",
            CHALLTESTSRV,
            "-defaultIPv4",
            &ip,
            "-defaultIPv6",
            "",
            "-http01",
            "",
            "-https01",
            "",
            "-tlsalpn01",
            "",
            "-doh",
            "",
        ]);
        p.names.push(dns.clone());
        let config = format!(
            r#"{{"pebble":{{"listenAddress":"0.0.0.0:14000","managementListenAddress":"0.0.0.0:15000",
            "certificate":"test/certs/localhost/cert.pem","privateKey":"test/certs/localhost/key.pem",
            "httpPort":{http_port},"tlsPort":{tls_port},"ocspResponderURL":"",
            "externalAccountBindingRequired":false,"domainBlocklist":["blocked-domain.example"],"retryAfter":{{"authz":1,"order":1}},
            "keyAlgorithm":"ecdsa",
            "profiles":{{"default":{{"description":"test","validityPeriod":{validity_secs}}}}}}}}}"#
        );
        let config_file = dir.join("pebble-config.json");
        std::fs::write(&config_file, config).unwrap();
        docker(&[
            "create",
            "--name",
            &pebble,
            "--network",
            &network,
            "-p",
            "127.0.0.1::14000",
            "-p",
            "127.0.0.1::15000",
            "-e",
            "PEBBLE_VA_NOSLEEP=1",
            "-e",
            "PEBBLE_WFE_NONCEREJECT=0",
            PEBBLE,
            "-config",
            "/test/edgefront-config.json",
            "-dnsserver",
            "challtestsrv:8053",
        ]);
        p.names.push(pebble.clone());
        docker(&[
            "cp",
            &config_file.display().to_string(),
            &format!("{pebble}:/test/edgefront-config.json"),
        ]);
        docker(&["start", &pebble]);
        docker(&[
            "cp",
            &format!("{pebble}:/test/certs/pebble.minica.pem"),
            &p.api_ca.display().to_string(),
        ]);
        let api = published(&pebble, 14000);
        p.directory = format!("https://localhost:{api}/dir");
        p.management = format!("https://localhost:{}", published(&pebble, 15000));
        p.dns_management = format!("http://127.0.0.1:{}", published(&dns, 8055));
        // Wait for the API to listen.
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(("127.0.0.1", api)).is_err() {
            assert!(Instant::now() < deadline, "pebble did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        p
    }

    /// The root Pebble signs certificates with, PEM, written to a file.
    pub fn root_file(&self) -> PathBuf {
        let path = self.dir.join("root.pem");
        let url = format!("{}/roots/0", self.management);
        let ca = self.api_ca.display().to_string();
        let out = Command::new("curl")
            .args(["-fsS", "--cacert", &ca, &url])
            .output()
            .unwrap();
        assert!(out.status.success(), "{url}");
        std::fs::write(&path, out.stdout).unwrap();
        path
    }

    /// A DNS-01 hook command that sets and clears TXT records through
    /// challtestsrv: `<hook> set|clear <fqdn> <value>`.
    pub fn dns_hook(&self) -> Vec<String> {
        let script = self.dir.join("dns-hook.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\ncase \"$1\" in\n  set) path=set-txt; body=\"{{\\\"host\\\":\\\"$2\\\",\\\"value\\\":\\\"$3\\\"}}\" ;;\n  clear) path=clear-txt; body=\"{{\\\"host\\\":\\\"$2\\\"}}\" ;;\n  *) exit 2 ;;\nesac\ncurl -fsS -X POST -d \"$body\" {}/$path\n",
                self.dns_management
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        vec![script.display().to_string()]
    }
}

/// A free TCP port on all interfaces, for a listener Pebble must reach.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
