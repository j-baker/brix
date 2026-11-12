//! Inputs are fetched in parallel, and fetches are reported as progress.
//!
//! A local HTTP server serves the inputs' tarballs and holds every
//! response until the expected number of requests have arrived (or ten
//! seconds pass), so fetching one input after the other shows up as one
//! request in flight at a time.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use brix::{Config, FetchConfig, Format, LockFlags, Session, StoreDir};
use brix_flake::{ActivityId, ActivityKind, Progress};
use brix_store::{nar, CanonPath, HashAlgo, HashFormat, MemoryAccessor};

/// How long the server holds a response for the others to arrive.
const PATIENCE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ServerState {
    /// Requests since the last [`Server::expect_together`].
    arrived: usize,
    in_flight: usize,
    max_in_flight: usize,
    /// Responses are held until this many requests have arrived.
    release_after: usize,
    /// Then each is held this much longer.
    hold: Duration,
    paths: Vec<String>,
}

struct Server {
    port: u16,
    state: Arc<(Mutex<ServerState>, Condvar)>,
}

impl Server {
    /// Serves `files` by path (`/a.tar.gz`), 404 for anything else.
    fn start(files: BTreeMap<String, Vec<u8>>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new((Mutex::new(ServerState::default()), Condvar::new()));
        let files = Arc::new(files);
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (files, state) = (files.clone(), shared.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    reader.read_line(&mut request).unwrap();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let path = request.split(' ').nth(1).unwrap_or("").to_owned();
                    let hold = {
                        let (lock, cv) = &*state;
                        let mut s = lock.lock().unwrap();
                        s.arrived += 1;
                        s.in_flight += 1;
                        s.max_in_flight = s.max_in_flight.max(s.in_flight);
                        s.paths.push(path.clone());
                        cv.notify_all();
                        let (s, _) = cv
                            .wait_timeout_while(s, PATIENCE, |s| s.arrived < s.release_after)
                            .unwrap();
                        s.hold
                    };
                    std::thread::sleep(hold);
                    state.0.lock().unwrap().in_flight -= 1;
                    let response = match files.get(&path) {
                        Some(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = stream.write_all(&response);
                });
            }
        });
        Server { port, state }
    }

    /// From now on, hold responses until `n` requests have arrived.
    fn expect_together(&self, n: usize) {
        *self.state.0.lock().unwrap() = ServerState {
            release_after: n,
            ..ServerState::default()
        };
    }

    /// From now on, hold each response for `hold`, so that transfers
    /// that could overlap do.
    fn hold_each(&self, hold: Duration) {
        *self.state.0.lock().unwrap() = ServerState {
            release_after: 1,
            hold,
            ..ServerState::default()
        };
    }

    fn max_in_flight(&self) -> usize {
        self.state.0.lock().unwrap().max_in_flight
    }

    fn paths(&self) -> Vec<String> {
        let mut paths = self.state.0.lock().unwrap().paths.clone();
        paths.sort();
        paths
    }

    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{name}.tar.gz", self.port)
    }
}

/// A gzipped tarball with one top-level directory holding `hello`, and
/// the NAR hash of what it unpacks to.
fn tarball(text: &str) -> (Vec<u8>, String) {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(text.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(1);
    header.set_cksum();
    archive
        .append_data(&mut header, "src/hello", text.as_bytes())
        .unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&archive.into_inner().unwrap()).unwrap();
    let mut tree = MemoryAccessor::new();
    tree.add_file(
        &CanonPath::new("/hello").unwrap(),
        text.as_bytes().to_vec(),
        false,
    )
    .unwrap();
    let (hash, _) = nar::hash(&tree, &CanonPath::root(), HashAlgo::Sha256).unwrap();
    (gz.finish().unwrap(), hash.to_string(HashFormat::Sri, true))
}

/// Records every report, and how many fetches ran at once.
#[derive(Debug, Default)]
struct Recorder(Mutex<Recorded>);

#[derive(Debug, Default)]
struct Recorded {
    next: u64,
    running: BTreeMap<u64, (ActivityKind, String)>,
    started: Vec<(ActivityKind, String)>,
    /// The last update of each transfer: (text, done, expected).
    transfers: BTreeMap<String, (u64, u64)>,
    max_fetches: usize,
    waits: i64,
    min_waits_at_start: i64,
}

impl Progress for Recorder {
    fn start(&self, kind: ActivityKind, text: &str) -> ActivityId {
        let mut r = self.0.lock().unwrap();
        r.next += 1;
        let id = r.next;
        r.running.insert(id, (kind, text.to_owned()));
        r.started.push((kind, text.to_owned()));
        let fetches = r
            .running
            .values()
            .filter(|(k, _)| *k == ActivityKind::Fetch)
            .count();
        r.max_fetches = r.max_fetches.max(fetches);
        ActivityId(id)
    }

    fn update(&self, id: ActivityId, done: u64, expected: u64) {
        let mut r = self.0.lock().unwrap();
        let text = r.running[&id.0].1.clone();
        r.transfers.insert(text, (done, expected));
    }

    fn stop(&self, id: ActivityId) {
        self.0.lock().unwrap().running.remove(&id.0);
    }

    fn begin_wait(&self) {
        self.0.lock().unwrap().waits += 1;
    }

    fn end_wait(&self) {
        let mut r = self.0.lock().unwrap();
        r.waits -= 1;
        r.min_waits_at_start = r.min_waits_at_start.min(r.waits);
    }
}

fn session(tmp: &Path, cache: &str, progress: Arc<Recorder>) -> Session {
    Session::new(Config {
        eval: brix_eval::Config {
            store_dir: StoreDir::new(tmp.join("store").to_str().unwrap()),
            ..brix_eval::Config::default()
        },
        fetch: FetchConfig {
            cache_dir: Some(tmp.join(cache)),
            ..FetchConfig::default()
        },
        progress,
        ..Config::default()
    })
    .unwrap()
}

fn write_flake(dir: &Path, server: &Server, names: &[&str]) {
    let inputs: String = names
        .iter()
        .map(|n| {
            format!(
                "  inputs.{n} = {{ url = \"tarball+{}\"; flake = false; }};\n",
                server.url(n)
            )
        })
        .collect();
    let read = |n: &str| format!("builtins.readFile \"${{inputs.{n}}}/hello\"");
    std::fs::write(
        dir.join("flake.nix"),
        format!(
            "{{\n{inputs}  outputs = inputs: {{\n    abc = {} + {} + {};\n    a = {};\n    ad = builtins.seq ({}) ({});\n    d = {};\n  }};\n}}\n",
            read("a"),
            read("b"),
            read("c"),
            read("a"),
            read("a"),
            read("d"),
            read("d"),
        ),
    )
    .unwrap();
}

#[test]
fn inputs_are_fetched_in_parallel_with_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let mut files = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for name in ["a", "b", "c"] {
        let (gz, hash) = tarball(&format!("{name}\n"));
        files.insert(format!("/{name}.tar.gz"), gz);
        hashes.insert(name, hash);
    }
    let sizes: BTreeMap<String, u64> = files
        .iter()
        .map(|(p, b)| (p.clone(), b.len() as u64))
        .collect();
    let server = Server::start(files);
    let flake = tmp.path().join("flake");
    std::fs::create_dir_all(&flake).unwrap();
    write_flake(&flake, &server, &["a", "b", "c"]);

    // Locking announces the three fetches before making the first.
    let progress = Arc::new(Recorder::default());
    let s = session(tmp.path(), "cache-lock", progress.clone());
    let flake_ref = s
        .parse_flake_ref(&format!("path:{}", flake.display()), "/")
        .unwrap();
    server.expect_together(3);
    let locked = s
        .flake_with_flags(&flake_ref, &LockFlags::default(), true)
        .unwrap();
    assert_eq!(server.max_in_flight(), 3, "{:?}", server.paths());
    assert_eq!(server.paths(), ["/a.tar.gz", "/b.tar.gz", "/c.tar.gz"]);
    let lock_text = locked.locked().lock_file.to_json_string();
    for hash in hashes.values() {
        assert!(lock_text.contains(hash.as_str()), "{lock_text}");
    }
    drop(locked);
    {
        let r = progress.0.lock().unwrap();
        assert_eq!(r.max_fetches, 3);
        let fetched: Vec<&str> = r
            .started
            .iter()
            .filter(|(k, _)| *k == ActivityKind::Fetch)
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(fetched.len(), 3, "{fetched:?}");
        assert!(
            fetched
                .iter()
                .all(|t| t.starts_with("fetching 'http://127.0.0.1:")),
            "{fetched:?}"
        );
        // Every transfer reported its whole length, which it knew.
        for name in ["a", "b", "c"] {
            let text = format!("downloading '{}'", server.url(name));
            let size = sizes[&format!("/{name}.tar.gz")];
            assert_eq!(r.transfers.get(&text), Some(&(size, size)), "{text}");
        }
        assert_eq!(r.waits, 0, "every wait ended");
        assert!(r.running.is_empty(), "every activity stopped");
    }
    std::fs::write(flake.join("flake.lock"), &lock_text).unwrap();

    // Evaluating with the lock file (and an empty cache): `abc` needs `a`
    // first, and the other inputs of the flake come with it.
    let s = session(tmp.path(), "cache-eval", Arc::new(Recorder::default()));
    server.expect_together(3);
    let value = s
        .flake(&flake_ref)
        .unwrap()
        .eval_attr_to_string("abc", Format::Raw)
        .unwrap();
    assert_eq!(value, "a\nb\nc\n");
    assert_eq!(server.max_in_flight(), 3, "{:?}", server.paths());
    assert_eq!(server.paths(), ["/a.tar.gz", "/b.tar.gz", "/c.tar.gz"]);

    // Again with the cache filled: nothing to fetch, nothing reported.
    let progress = Arc::new(Recorder::default());
    let s = session(tmp.path(), "cache-eval", progress.clone());
    server.expect_together(1);
    let value = s
        .flake(&flake_ref)
        .unwrap()
        .eval_attr_to_string("abc", Format::Raw)
        .unwrap();
    assert_eq!(value, "a\nb\nc\n");
    assert!(server.paths().is_empty(), "{:?}", server.paths());
    assert!(progress.0.lock().unwrap().started.is_empty());
}

/// A background fetch that fails is an error only if evaluation needs the
/// input, and then the same error as fetching it in the foreground.
#[test]
fn a_failed_background_fetch_fails_only_where_it_is_needed() {
    let tmp = tempfile::tempdir().unwrap();
    let (gz, hash) = tarball("a\n");
    let mut files = BTreeMap::new();
    for name in ["a", "b", "c"] {
        files.insert(format!("/{name}.tar.gz"), gz.clone());
    }
    let server = Server::start(files);
    server.expect_together(1);
    let flake = tmp.path().join("flake");
    std::fs::create_dir_all(&flake).unwrap();
    write_flake(&flake, &server, &["a", "b", "c", "d"]);
    // `d` is served nowhere; the lock pins the content `a` has.
    let node = |name: &str| {
        format!(
            r#""{name}": {{ "flake": false, "locked": {{ "narHash": "{hash}", "type": "tarball", "url": "{url}" }}, "original": {{ "type": "tarball", "url": "{url}" }} }}"#,
            url = server.url(name)
        )
    };
    std::fs::write(
        flake.join("flake.lock"),
        format!(
            r#"{{ "nodes": {{ {}, {}, {}, {}, "root": {{ "inputs": {{ "a": "a", "b": "b", "c": "c", "d": "d" }} }} }}, "root": "root", "version": 7 }}"#,
            node("a"),
            node("b"),
            node("c"),
            node("d")
        ),
    )
    .unwrap();

    let eval = |cache: &str, attr: &str| {
        let s = session(tmp.path(), cache, Arc::new(Recorder::default()));
        let flake_ref = s
            .parse_flake_ref(&format!("path:{}", flake.display()), "/")
            .unwrap();
        let result = s
            .flake(&flake_ref)
            .unwrap()
            .eval_attr_to_string(attr, Format::Raw);
        result.map_err(|e| e.to_string())
    };
    // `d` is fetched in the background along with `a`, and fails there.
    assert_eq!(eval("cache-1", "a").unwrap(), "a\n");
    // Needed after `a`: the background fetch's error.
    let background = eval("cache-2", "ad").unwrap_err();
    // Needed first: fetched in the foreground.
    let foreground = eval("cache-3", "d").unwrap_err();
    assert!(background.contains("HTTP error 404"), "{background}");
    // The traces differ above the fetch, where the calls differ.
    let from_fetch = |e: &str| {
        let at = e.find("while calling the 'fetchTree' builtin").expect(e);
        e[at..].to_owned()
    };
    assert_eq!(from_fetch(&background), from_fetch(&foreground));
}

/// With `http-connections = 1` one transfer runs at a time, foreground
/// and background together (Nix's limit is on all transfers), and a
/// session dropped while some are still queued ends cleanly: they are
/// abandoned, not started on a closed store.
#[test]
fn one_connection_and_a_session_dropped_with_fetches_queued() {
    let tmp = tempfile::tempdir().unwrap();
    let (gz, hash) = tarball("a\n");
    let names = ["a", "b", "c", "d"];
    let files: BTreeMap<String, Vec<u8>> = names
        .iter()
        .map(|n| (format!("/{n}.tar.gz"), gz.clone()))
        .collect();
    let server = Server::start(files);
    let flake = tmp.path().join("flake");
    std::fs::create_dir_all(&flake).unwrap();
    write_flake(&flake, &server, &names);
    let node = |name: &str| {
        format!(
            r#""{name}": {{ "flake": false, "locked": {{ "narHash": "{hash}", "type": "tarball", "url": "{url}" }}, "original": {{ "type": "tarball", "url": "{url}" }} }}"#,
            url = server.url(name)
        )
    };
    let nodes: Vec<String> = names.iter().map(|n| node(n)).collect();
    std::fs::write(
        flake.join("flake.lock"),
        format!(
            r#"{{ "nodes": {{ {}, "root": {{ "inputs": {{ "a": "a", "b": "b", "c": "c", "d": "d" }} }} }}, "root": "root", "version": 7 }}"#,
            nodes.join(", ")
        ),
    )
    .unwrap();
    let one_connection = |cache: &str| {
        Session::new(Config {
            eval: brix_eval::Config {
                store_dir: StoreDir::new(tmp.path().join("store").to_str().unwrap()),
                ..brix_eval::Config::default()
            },
            fetch: FetchConfig {
                cache_dir: Some(tmp.path().join(cache)),
                http_connections: 1,
                ..FetchConfig::default()
            },
            ..Config::default()
        })
        .unwrap()
    };
    // `a` in the foreground, then (on the one thread) `b`, never at once;
    // `c` and `d` queue behind it.
    server.hold_each(Duration::from_millis(300));
    let s = one_connection("cache-1");
    let flake_ref = s
        .parse_flake_ref(&format!("path:{}", flake.display()), "/")
        .unwrap();
    let value = s
        .flake(&flake_ref)
        .unwrap()
        .eval_attr_to_string("a", Format::Raw)
        .unwrap();
    assert_eq!(value, "a\n");
    drop(s);
    assert_eq!(server.max_in_flight(), 1, "{:?}", server.paths());
    // The cache the abandoned session filled serves the next one.
    let s = one_connection("cache-1");
    let flake_ref = s
        .parse_flake_ref(&format!("path:{}", flake.display()), "/")
        .unwrap();
    let value = s
        .flake(&flake_ref)
        .unwrap()
        .eval_attr_to_string("abc", Format::Raw)
        .unwrap();
    assert_eq!(value, "a\na\na\n");
}
