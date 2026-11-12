//! An HTTPS server for tests, with a certificate for `127.0.0.1` signed by
//! a test CA (`tests/fixtures/tls`: `ca.pem` signed `server.pem`, whose
//! key is `server.key`; `other-ca.pem` signed nothing here). They were
//! made with `openssl` (P-256 keys, valid for a hundred years):
//!
//! ```text
//! openssl req -x509 -new -key ca.key -out ca.pem -days 36500 -subj "/CN=brix test CA" \
//!   -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
//! openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -set_serial 1 -out server.pem \
//!   -days 36500 -extfile <(printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\n…serverAuth\n')
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// A request as the server read it.
pub(crate) struct Request {
    pub method: String,
    /// The path and query (`/repo.git/info/refs?service=…`).
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A response: status, header lines (`Name: value`), body.
pub(crate) type Response = (u16, Vec<String>, Vec<u8>);

/// Serves HTTPS on a port of 127.0.0.1, answering each request (one per
/// connection) with `handler`, until the test ends. Returns the base URL.
pub(crate) fn serve(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> String {
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
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            let handler = handler.clone();
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).expect("TLS connection");
                let tls = rustls::StreamOwned::new(conn, stream);
                // A client that refuses the certificate ends the handshake:
                // nothing to answer then.
                let mut reader = BufReader::new(tls);
                if let Some(request) = read_request(&mut reader) {
                    let tls = reader.get_mut();
                    let (status, headers, body) = handler(&request);
                    let mut out = format!("HTTP/1.1 {status} X\r\n");
                    for h in headers {
                        out.push_str(&h);
                        out.push_str("\r\n");
                    }
                    out.push_str(&format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ));
                    let _ = tls.write_all(out.as_bytes());
                    let _ = tls.write_all(&body);
                    let _ = tls.flush();
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                }
            });
        }
    });
    url
}

type Tls = rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>;

/// Reads one request (a `Content-Length` or chunked body).
fn read_request(reader: &mut BufReader<Tls>) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    if let Some(len) = request.header("content-length") {
        let mut body = vec![0; len.parse().ok()?];
        reader.read_exact(&mut body).ok()?;
        request.body = body;
    } else if request
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            request.body.extend_from_slice(&chunk[..size]);
        }
    }
    Some(request)
}

/// `git http-backend` run as a CGI program for `request`, serving the
/// repositories below `root`.
pub(crate) fn git_http_backend(root: &Path, request: &Request) -> Response {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    let mut cmd = std::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    if let Some(t) = request.header("content-type") {
        cmd.env("CONTENT_TYPE", t);
    }
    if let Some(p) = request.header("git-protocol") {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = cmd.spawn().expect("the `git` command must be on PATH");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(&request.body)
        .expect("write the request body");
    let out = child.wait_with_output().expect("run git http-backend");
    let split = out
        .stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| {
            out.stdout
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, 2))
        })
        .expect("CGI headers end with an empty line");
    let head = String::from_utf8_lossy(&out.stdout[..split.0]).into_owned();
    let body = out.stdout[split.0 + split.1..].to_vec();
    let mut status = 200;
    let mut headers = Vec::new();
    for line in head.lines() {
        match line.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("status") => {
                status = value.trim()[..3].parse().expect("a status code");
            }
            Some(_) => headers.push(line.to_owned()),
            None => {}
        }
    }
    (status, headers, body)
}
