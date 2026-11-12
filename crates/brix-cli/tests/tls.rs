//! HTTPS downloads trust what Nix trusts: the CA file `ssl-cert-file`
//! names (by default `NIX_SSL_CERT_FILE`, `SSL_CERT_FILE` or a system
//! bundle; `nix.conf` and the command line win over the environment), and
//! only it. Compared with the reference `nix` (`BRIX_NIX_BIN`) on a local
//! HTTPS server whose certificate a test CA signed
//! (`tests/fixture-tls`, copies of brix-flake's `tests/fixtures/tls`).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use assert_cmd::prelude::*;

fn nix_bin() -> PathBuf {
    PathBuf::from(
        std::env::var("BRIX_NIX_BIN").expect(
            "BRIX_NIX_BIN must point at the reference nix binary (run inside `nix develop`)",
        ),
    )
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixture-tls")
        .join(name)
}

/// Serves `hello` over HTTPS for every path, one request per connection,
/// until the test ends. Returns the base URL.
fn serve() -> String {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(fixture("server.pem"))
        .expect("server.pem")
        .collect::<Result<Vec<_>, _>>()
        .expect("server.pem holds certificates");
    let key = PrivateKeyDer::from_pem_file(fixture("server.key")).expect("server.key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .expect("the server certificate and key match");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("https://{}", listener.local_addr().expect("address"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).expect("TLS connection");
                let mut reader = BufReader::new(rustls::StreamOwned::new(conn, stream));
                // A client that refuses the certificate ends the handshake
                // here, with nothing to answer.
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                let tls = reader.get_mut();
                let body = "hello\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(response.as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    url
}

/// Status, stdout and stderr of `nix flake prefetch` of `url` (a `file`
/// input) by `cmd`, with `env` (both certificate variables are cleared
/// first) and `options` (`--option` pairs). One attempt, no global
/// registry, caches of the run's own.
fn prefetch(
    mut cmd: Command,
    home: &Path,
    url: &str,
    env: &[(&str, &Path)],
    options: &[(&str, &str)],
) -> (i32, String, String) {
    cmd.env_remove("NIX_SSL_CERT_FILE")
        .env_remove("SSL_CERT_FILE")
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env(
            "NIX_CONFIG",
            "experimental-features = nix-command flakes\nflake-registry =\nfiletransfer-retry-attempts = 1",
        );
    for (k, v) in env {
        cmd.env(k, v);
    }
    for (name, value) in options {
        cmd.args(["--option", name, value]);
    }
    let out = cmd
        .args(["flake", "prefetch", &format!("file+{url}")])
        .output()
        .expect("run command");
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| !brix_testutil::is_noise(l.as_bytes()))
        .map(|l| format!("{l}\n"))
        .collect();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    )
}

#[test]
fn https_downloads_trust_ssl_cert_file_as_nix_does() {
    let base = serve();
    let dir = tempfile::tempdir().unwrap();
    let ca = fixture("ca.pem");
    let missing = dir.path().join("missing.pem");
    let not_a_bundle = fixture("server.key");
    type Case<'a> = (&'a str, Vec<(&'a str, &'a Path)>, Vec<(&'a str, &'a str)>);
    let ca_str = ca.to_str().unwrap();
    let cases: Vec<Case<'_>> = vec![
        (
            "NIX_SSL_CERT_FILE",
            vec![("NIX_SSL_CERT_FILE", &ca)],
            vec![],
        ),
        ("SSL_CERT_FILE", vec![("SSL_CERT_FILE", &ca)], vec![]),
        (
            "NIX_SSL_CERT_FILE wins",
            vec![("NIX_SSL_CERT_FILE", &missing), ("SSL_CERT_FILE", &ca)],
            vec![],
        ),
        (
            "a missing file",
            vec![("NIX_SSL_CERT_FILE", &missing)],
            vec![],
        ),
        (
            "a file without certificates",
            vec![("NIX_SSL_CERT_FILE", &not_a_bundle)],
            vec![],
        ),
        (
            "the setting wins",
            vec![("NIX_SSL_CERT_FILE", &missing)],
            vec![("ssl-cert-file", ca_str)],
        ),
        // Without a CA file, curl's default store, which reads the
        // variable itself in Nix as nixpkgs builds it.
        (
            "an empty setting",
            vec![("NIX_SSL_CERT_FILE", &ca)],
            vec![("ssl-cert-file", "")],
        ),
        ("the system's", vec![], vec![]),
    ];
    for (i, (what, env, options)) in cases.iter().enumerate() {
        // A URL of its own: nothing is served from a cache.
        let url = format!("{base}/{i}");
        let ours = prefetch(
            Command::cargo_bin("brix").expect("brix binary"),
            &dir.path().join(format!("brix-{i}")),
            &url,
            env,
            options,
        );
        let theirs = prefetch(
            Command::new(nix_bin()),
            &dir.path().join(format!("nix-{i}")),
            &url,
            env,
            options,
        );
        assert_eq!(ours, theirs, "{what}: brix (left) and nix (right) differ");
    }
}
