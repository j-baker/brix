//! HTTP fetches send `netrc-file` credentials as basic authentication,
//! the way curl's optional netrc does for Nix: only to hosts the file
//! names, and only when the request has no `Authorization` of its own.

#![cfg(feature = "native")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use brix_flake::{Attr, Attrs, FetchSettings, Fetcher, Input, NativeFetcher};

/// A gzipped tarball with one top-level directory holding `hello.txt`.
fn tarball() -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let data = b"hello from behind basic auth\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(1);
    header.set_cksum();
    archive
        .append_data(&mut header, "src-1.0/hello.txt", &data[..])
        .unwrap();
    let tar = archive.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// Serves `/src.tar.gz` to requests carrying `expected_auth`, answering
/// 401 to everything else. Returns the port; the server lives in a
/// thread for the test's duration.
fn serve(expected_auth: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = Arc::new(tarball());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let body = body.clone();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut authorised = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                // Header names are case-insensitive (ureq sends them in lower case).
                if let Some((name, v)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("authorization") {
                        authorised = v.trim() == expected_auth;
                    }
                }
            }
            let response = if authorised {
                let mut r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                r.extend_from_slice(&body);
                r
            } else {
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    port
}

fn tarball_input(settings: &FetchSettings, port: u16) -> Input {
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::String("tarball".into()));
    attrs.insert(
        "url".into(),
        Attr::String(format!("http://127.0.0.1:{port}/src.tar.gz")),
    );
    Input::from_attrs(settings, attrs).unwrap()
}

fn fetcher(dir: &std::path::Path) -> NativeFetcher {
    NativeFetcher::new()
        .unwrap()
        .with_cache_dir(dir.to_path_buf())
}

#[test]
fn netrc_credentials_are_sent_for_matching_hosts() {
    // `user:s3cr3t`
    let port = serve("Basic dXNlcjpzM2NyM3Q=");
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("netrc");
    std::fs::write(&netrc, "machine 127.0.0.1 login user password s3cr3t\n").unwrap();
    let settings = FetchSettings {
        netrc_file: Some(netrc),
        ..FetchSettings::default()
    };
    let input = tarball_input(&settings, port);
    let tree = fetcher(dir.path()).fetch(&settings, &input).unwrap();
    assert_eq!(
        tree.accessor
            .read_file(&brix_store::CanonPath::new("/hello.txt").unwrap())
            .unwrap(),
        b"hello from behind basic auth\n"
    );
}

#[test]
fn no_netrc_or_other_host_means_no_credentials() {
    let port = serve("Basic dXNlcjpzM2NyM3Q=");
    let dir = tempfile::tempdir().unwrap();
    // No netrc at all.
    let settings = FetchSettings::default();
    let input = tarball_input(&settings, port);
    let err = fetcher(dir.path())
        .fetch(&settings, &input)
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP error 401"), "{err}");
    // A netrc that names a different machine.
    let netrc = dir.path().join("netrc");
    std::fs::write(&netrc, "machine example.org login user password s3cr3t\n").unwrap();
    let settings = FetchSettings {
        netrc_file: Some(netrc),
        ..FetchSettings::default()
    };
    let err = fetcher(dir.path())
        .fetch(&settings, &input)
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP error 401"), "{err}");
    // A `default` entry applies to any host.
    let netrc = dir.path().join("netrc2");
    std::fs::write(&netrc, "default login user password s3cr3t\n").unwrap();
    let settings = FetchSettings {
        netrc_file: Some(netrc),
        ..FetchSettings::default()
    };
    fetcher(dir.path()).fetch(&settings, &input).unwrap();
}
