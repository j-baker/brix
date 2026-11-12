//! A Git LFS server for tests: the batch API and the objects, over plain
//! HTTP on 127.0.0.1, so that brix and the reference `nix` can both fetch
//! from it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// The `Authorization` the batch API hands out for downloads, and the
/// object URLs want.
pub const LFS_AUTHORIZATION: &str = "Bearer lfs-test-token";

/// A request the server got.
#[derive(Clone, Debug)]
pub struct LfsRequest {
    pub method: String,
    /// The path and query (`/repo.git/info/lfs/objects/batch`).
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl LfsRequest {
    /// The value of header `name` (any case), if sent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// How the batch API answers.
#[derive(Clone, Copy, Debug)]
pub enum LfsMode {
    /// With a download action for each object asked for.
    Objects,
    /// With this status and body.
    Status(u16, &'static str),
    /// With an error object for each, as for objects it does not have.
    ErrorObject,
    /// With each object's size plus one.
    WrongSize,
    /// With downloads whose last byte is changed.
    Corrupt,
    /// With downloads one byte short.
    Short,
}

/// The server: any `POST` is a batch request (`…/objects/batch`), and
/// `GET <base>/objects/<oid>` downloads an object (with
/// [`LFS_AUTHORIZATION`]). Every request is recorded. It serves until the
/// test process ends.
pub struct LfsServer {
    base: String,
    requests: Arc<Mutex<Vec<LfsRequest>>>,
}

impl LfsServer {
    /// Serves `objects`, each its SHA-256 in hexadecimal and its contents.
    pub fn start(objects: Vec<(String, Vec<u8>)>, mode: LfsMode) -> LfsServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the listener's address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let objects: Arc<HashMap<String, Vec<u8>>> = Arc::new(objects.into_iter().collect());
        let (seen, url) = (requests.clone(), base.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (seen, url, objects) = (seen.clone(), url.clone(), objects.clone());
                std::thread::spawn(move || serve(stream, &seen, &url, &objects, mode));
            }
        });
        LfsServer { base, requests }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The requests so far, in the order they came.
    pub fn requests(&self) -> Vec<LfsRequest> {
        self.requests.lock().expect("not poisoned").clone()
    }
}

fn serve(
    stream: TcpStream,
    seen: &Mutex<Vec<LfsRequest>>,
    base: &str,
    objects: &HashMap<String, Vec<u8>>,
    mode: LfsMode,
) {
    let mut reader = BufReader::new(stream);
    let Some(request) = read_request(&mut reader) else {
        return;
    };
    seen.lock().expect("not poisoned").push(request.clone());
    let (status, content_type, body) = answer(&request, base, objects, mode);
    let mut stream = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // A client that went away is no failure of the server.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

/// One request, with a `Content-Length` body.
fn read_request(reader: &mut BufReader<TcpStream>) -> Option<LfsRequest> {
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
    let mut request = LfsRequest {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    if let Some(len) = request.header("content-length") {
        let mut body = vec![0; len.parse().ok()?];
        reader.read_exact(&mut body).ok()?;
        request.body = body;
    }
    Some(request)
}

const LFS_JSON: &str = "application/vnd.git-lfs+json";

fn answer(
    request: &LfsRequest,
    base: &str,
    objects: &HashMap<String, Vec<u8>>,
    mode: LfsMode,
) -> (u16, &'static str, Vec<u8>) {
    if request.method == "GET" {
        let oid = request.target.trim_start_matches("/objects/");
        let Some(data) = objects.get(oid) else {
            return (404, "text/plain", b"no such object".to_vec());
        };
        if request.header("authorization") != Some(LFS_AUTHORIZATION) {
            return (401, "text/plain", b"who are you".to_vec());
        }
        let mut data = data.clone();
        match mode {
            LfsMode::Corrupt => {
                if let Some(last) = data.last_mut() {
                    *last ^= 1;
                }
            }
            LfsMode::Short => {
                data.pop();
            }
            _ => {}
        }
        return (200, "application/octet-stream", data);
    }
    if let LfsMode::Status(status, body) = mode {
        return (status, LFS_JSON, body.as_bytes().to_vec());
    }
    let objects: Vec<String> = asked_for(&String::from_utf8_lossy(&request.body))
        .into_iter()
        .map(|(oid, size)| match mode {
            LfsMode::ErrorObject => format!(
                r#"{{"oid":"{oid}","size":{size},"error":{{"code":404,"message":"Object does not exist"}}}}"#
            ),
            _ => {
                let size = if matches!(mode, LfsMode::WrongSize) {
                    size + 1
                } else {
                    size
                };
                format!(
                    r#"{{"oid":"{oid}","size":{size},"authenticated":true,"actions":{{"download":{{"href":"{base}/objects/{oid}","header":{{"Authorization":"{LFS_AUTHORIZATION}"}}}}}}}}"#
                )
            }
        })
        .collect();
    let body = format!(r#"{{"objects":[{}]}}"#, objects.join(","));
    (200, LFS_JSON, body.into_bytes())
}

/// The `oid` and `size` of each object a batch request (as Nix writes
/// it: `{"objects":[{"oid":"…","size":N}],…}`) asks for.
fn asked_for(body: &str) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(r#""oid":""#) {
        rest = &rest[start + 7..];
        let Some(end) = rest.find('"') else { break };
        let oid = rest[..end].to_owned();
        rest = &rest[end..];
        let size = match rest.find(r#""size":"#) {
            Some(at) => {
                let digits: String = rest[at + 7..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse().expect("a batch request gives sizes")
            }
            None => panic!("a batch request gives sizes: {body}"),
        };
        found.push((oid, size));
    }
    found
}
