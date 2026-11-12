//! `--offline`, the check for an Internet connection and
//! `connect-timeout`, against the reference `nix` (`BRIX_NIX_BIN`).
//!
//! Nix's `--offline` does not stop fetching: it turns substitution off and
//! takes every earlier download to be current (`tarball-ttl` at its
//! maximum), tries each download once, with a one-second connection
//! timeout, and still downloads what it never fetched (`disableNet` in
//! `src/nix/main.cc`). Each of those settings stays as it is when the user
//! set it (a user `nix.conf`, `NIX_CONFIG`, the command line; the system
//! `nix.conf` does not count). Before any command `nix` also looks for a
//! network interface other than loopback and link-local (or a proxy
//! variable); without one it warns and goes on as with `--offline`.
//!
//! The downloads come from a server on `127.0.0.1` that counts the
//! requests it answers. Every run gets the same configuration of its own:
//! a system `nix.conf` the test writes (`NIX_CONF_DIR`), no user file
//! (`NIX_USER_CONF_FILES`), the oracle's `NIX_CONFIG` without the global
//! registry, and caches of its own. To count as connected wherever the
//! tests run (the package's test run is a build sandbox without a
//! network), brix and `nix` both see `http_proxy` set to the empty string:
//! Nix takes any lower-case proxy variable as a connection, and curl and
//! ureq ignore an empty one. The tests of the check itself take the
//! network away with `unshare --user --map-root-user --net` (util-linux),
//! a namespace whose only interface is a loopback that is down.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use brix_testutil::{git, is_noise, GitRepo, Oracle, ORACLE_NIX_CONFIG};

/// What a run printed.
#[derive(Debug, PartialEq, Eq)]
struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

/// Runs `cmd`; `nix`'s stderr loses what [`is_noise`] recognises (its
/// crash reporter), except the warning these tests are about.
fn run(mut cmd: Command) -> Output {
    let out = cmd.output().expect("run command");
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| {
            l.starts_with("warning: you don't have Internet access") || !is_noise(l.as_bytes())
        })
        .map(|l| format!("{l}\n"))
        .collect();
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    }
}

/// The proxy variables curl, ureq or Nix's check read.
const PROXY_VARIABLES: [&str; 10] = [
    "http_proxy",
    "https_proxy",
    "ftp_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "FTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];

/// The configuration the runs of one test share: a directory with the
/// system `nix.conf`, and caches by program.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    /// A system `nix.conf` of `system_conf`.
    fn new(system_conf: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/nix.conf"), system_conf).unwrap();
        Env { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The variables both programs get; `who` keeps their caches apart.
    /// No proxy variable is set (see [`connected`]).
    fn apply(&self, cmd: &mut Command, who: &str) {
        for var in PROXY_VARIABLES {
            cmd.env_remove(var);
        }
        cmd.env("NIX_CONF_DIR", self.path().join("etc"))
            .env("NIX_USER_CONF_FILES", self.path().join("no-user.conf"))
            .env("XDG_CACHE_HOME", self.path().join(format!("cache-{who}")))
            .env("TZ", "UTC");
    }

    /// `brix` with `args`, on the dummy store, deciding for itself
    /// whether it is connected.
    fn brix(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("brix").expect("brix binary");
        self.apply(&mut cmd, "brix");
        cmd.env_remove("BRIX_HAVE_INTERNET")
            .env(
                "NIX_CONFIG",
                format!("{ORACLE_NIX_CONFIG}\nflake-registry ="),
            )
            .args(["--store", "dummy://"])
            .args(args);
        cmd
    }

    /// The reference `nix` with `args`, on the dummy store.
    fn nix(&self, args: &[&str]) -> Command {
        let mut cmd = Oracle::new()
            .config("flake-registry =")
            .args(["--store", "dummy://"])
            .args(args)
            .command();
        self.apply(&mut cmd, "nix");
        cmd
    }
}

/// `cmd` as a machine with an Internet connection runs it, by Nix's
/// check: with a lower-case proxy variable, empty so that nothing uses
/// it.
fn connected(mut cmd: Command) -> Command {
    cmd.env("http_proxy", "");
    cmd
}

/// `cmd` without a network: in a user and network namespace of its own,
/// whose only interface is a loopback that is down. Same arguments,
/// environment and directory.
fn without_network(cmd: &Command) -> Command {
    let mut wrapped = Command::new("unshare");
    wrapped
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => wrapped.env(key, value),
            None => wrapped.env_remove(key),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        wrapped.current_dir(dir);
    }
    wrapped
}

/// An HTTP server on 127.0.0.1 answering `GET /NAME`, one request per
/// connection: `files` has the body of `NAME` (200), a `NAME` starting
/// with `fail` gets HTTP 500, anything else 404. Counts the requests.
struct Server {
    base: String,
    /// The paths asked for, in order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(files: Vec<(&str, Vec<u8>)>) -> Server {
        let files: Vec<(String, Vec<u8>)> = files
            .into_iter()
            .map(|(name, body)| (format!("/{name}"), body))
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                let path = request_line.split(' ').nth(1).unwrap_or("/").to_owned();
                let (status, body) = match files.iter().find(|(name, _)| *name == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None if path.starts_with("/fail") => {
                        ("500 Internal Server Error", b"no".to_vec())
                    }
                    None => ("404 Not Found", b"not found".to_vec()),
                };
                seen.lock()
                    .expect("request log poisoned")
                    .push(path.clone());
                let mut stream = reader.into_inner();
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                // A client that gave up has nothing to be told.
                let _ = stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(&body));
            }
        });
        Server { base, requests }
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    fn requests(&self) -> usize {
        self.requests.lock().expect("request log poisoned").len()
    }

    /// How many requests asked for `name`.
    fn requests_for(&self, name: &str) -> usize {
        let path = format!("/{name}");
        self.requests
            .lock()
            .expect("request log poisoned")
            .iter()
            .filter(|p| **p == path)
            .count()
    }

    /// Runs `cmd`: what it printed, and how many requests it made.
    fn run(&self, cmd: Command) -> (Output, usize) {
        let before = self.requests();
        let out = run(cmd);
        (out, self.requests() - before)
    }
}

/// A gzipped tarball of one directory, `name`, holding a flake. The flake
/// names `dir` in a comment, so that its tree, and its NAR hash, is the
/// test's own: no store path from elsewhere stands in for a download.
fn flake_tarball(dir: &Path, name: &str) -> Vec<u8> {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join(name)).unwrap();
    std::fs::write(
        src.join(name).join("flake.nix"),
        format!(
            "# {}\n{{ description = \"{name}\"; outputs = {{ self }}: {{ }}; }}\n",
            dir.display()
        ),
    )
    .unwrap();
    let tarball = dir.join(format!("{name}.tar.gz"));
    let status = Command::new("tar")
        .arg("-C")
        .arg(&src)
        .arg("-czf")
        .arg(&tarball)
        .arg(name)
        .status()
        .expect("the `tar` command must be on PATH");
    assert!(status.success());
    std::fs::read(&tarball).unwrap()
}

/// `--offline` uses what was downloaded before however old it is, and
/// downloads what was not; whoever set `tarball-ttl` (on the command
/// line here) is obeyed, and `--refresh` wins. The system `nix.conf` sets
/// `tarball-ttl = 0`, so that without `--offline` everything is asked
/// for again, and does not count as setting it.
#[test]
fn offline_uses_earlier_downloads_and_downloads_the_rest() {
    let env = Env::new("tarball-ttl = 0\n");
    let server = Server::start(vec![
        ("a.tar.gz", flake_tarball(env.path(), "a")),
        ("b.tar.gz", flake_tarball(env.path(), "b")),
    ]);
    let a = format!("tarball+{}", server.url("a.tar.gz"));
    let b = format!("tarball+{}", server.url("b.tar.gz"));
    let (a, b) = (a.as_str(), b.as_str());
    let steps: [(&str, Vec<&str>, &str, usize); 6] = [
        ("the first download", vec![], a, 1),
        ("offline, the earlier download", vec!["--offline"], a, 0),
        ("online, asked again", vec![], a, 1),
        ("offline, never downloaded", vec!["--offline"], b, 1),
        (
            "offline with tarball-ttl set",
            vec!["--offline", "--tarball-ttl", "0"],
            a,
            1,
        ),
        (
            "offline and --refresh",
            vec!["--offline", "--refresh"],
            a,
            1,
        ),
    ];
    for (what, flags, flake_ref, downloads) in steps {
        let mut args = flags.clone();
        args.extend(["flake", "metadata", "--json", flake_ref]);
        let (ours, our_requests) = server.run(connected(env.brix(&args)));
        let (theirs, their_requests) = server.run(connected(env.nix(&args)));
        assert_eq!(ours, theirs, "{what}: brix (left) and nix (right) differ");
        assert_eq!(
            (our_requests, their_requests),
            (downloads, downloads),
            "{what}: requests by brix and by nix"
        );
        assert_eq!(theirs.status, 0, "{what}: {}", theirs.stderr);
    }
}

/// `--offline` tries a download once (`filetransfer-retry-attempts` 0),
/// unless the attempts are set.
#[test]
fn offline_tries_a_download_once() {
    let env = Env::new("");
    let server = Server::start(Vec::new());
    let steps: [(&str, Vec<&str>, usize); 3] = [
        ("online", vec![], 5),
        ("offline", vec!["--offline"], 1),
        (
            "offline, attempts set",
            vec!["--offline", "--download-attempts", "2"],
            2,
        ),
    ];
    for (i, (what, flags, attempts)) in steps.into_iter().enumerate() {
        let flake_ref = format!("tarball+{}", server.url(&format!("fail-{i}.tar.gz")));
        let mut args = flags;
        args.extend([
            "--filetransfer-retry-delay",
            "1",
            "flake",
            "metadata",
            "--json",
            flake_ref.as_str(),
        ]);
        let (ours, our_requests) = server.run(connected(env.brix(&args)));
        let (theirs, their_requests) = server.run(connected(env.nix(&args)));
        let retries = |out: &Output| out.stderr.matches("; retrying in ").count();
        assert_eq!(
            (ours.status, our_requests, retries(&ours)),
            (theirs.status, their_requests, retries(&theirs)),
            "{what}: brix (left) and nix (right) differ\nbrix: {}\nnix: {}",
            ours.stderr,
            theirs.stderr
        );
        assert_eq!(their_requests, attempts, "{what}: {}", theirs.stderr);
        assert_eq!(theirs.status, 1, "{what}");
    }
}

/// curl's message for a connection that took too long, without the time
/// it waited, which its clock gives to the millisecond or so.
fn without_milliseconds(stderr: &str) -> String {
    let re = regex::Regex::new(r"timed out after \d+ milliseconds").unwrap();
    re.replace_all(stderr, "timed out after N milliseconds")
        .into_owned()
}

/// A server that takes connections and never answers the TLS handshake:
/// `--offline` gives up on it after a second (`connect-timeout` 1), with
/// curl's message; `--connect-timeout` sets the limit itself.
#[test]
fn offline_connects_with_a_one_second_timeout() {
    let env = Env::new("");
    // Never accepted: the kernel completes the TCP handshakes of its
    // backlog, and nothing ever answers the clients' TLS hello.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("https://{}/x", listener.local_addr().expect("address"));
    let file_ref = format!("file+{url}");
    let file_ref = file_ref.as_str();
    let ca = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture-tls/ca.pem");
    for (what, args, limit) in [
        (
            "offline",
            vec!["--offline", "flake", "prefetch", file_ref],
            Duration::from_secs(1),
        ),
        (
            "connect-timeout 2, one attempt",
            vec![
                "--connect-timeout",
                "2",
                "--filetransfer-retry-attempts",
                "1",
                "flake",
                "prefetch",
                file_ref,
            ],
            Duration::from_secs(2),
        ),
    ] {
        let timed = |mut cmd: Command| {
            cmd.env("NIX_SSL_CERT_FILE", &ca)
                .env_remove("SSL_CERT_FILE");
            let start = Instant::now();
            let out = run(connected(cmd));
            (out, start.elapsed())
        };
        let (ours, our_time) = timed(env.brix(&args));
        let (theirs, their_time) = timed(env.nix(&args));
        assert_eq!(
            (
                ours.status,
                ours.stdout.as_str(),
                without_milliseconds(&ours.stderr)
            ),
            (
                theirs.status,
                theirs.stdout.as_str(),
                without_milliseconds(&theirs.stderr)
            ),
            "{what}: brix (left) and nix (right) differ"
        );
        assert!(
            theirs
                .stderr
                .contains("Timeout was reached (28) Connection timed out"),
            "{what}: {}",
            theirs.stderr
        );
        // Nix's default limit is 15 s.
        for (who, time) in [("brix", our_time), ("nix", their_time)] {
            assert!(
                time >= limit.mul_f32(0.9) && time < limit + Duration::from_secs(8),
                "{what}: {who} took {time:?}"
            );
        }
    }
    drop(listener);
}

/// `nix flake lock` and `nix flake update` ask about unlocked inputs
/// again (`tarball-ttl` 0), `--offline` or not, where other commands use
/// what was downloaded within the hour.
#[test]
fn locking_asks_again_even_offline() {
    let env = Env::new("");
    let server = Server::start(vec![("a.tar.gz", flake_tarball(env.path(), "a"))]);
    let flake_nix = format!(
        "{{ inputs.a.url = \"tarball+{}\"; outputs = {{ self, a }}: {{ d = \"f\"; }}; }}\n",
        server.url("a.tar.gz")
    );
    // What runs, the flake reference's fragment, whether the lock file is
    // removed first, and how many downloads it makes.
    let steps: [(&str, Vec<&str>, &str, bool, usize); 4] = [
        ("lock", vec!["flake", "lock"], "", true, 1),
        (
            "lock again, offline",
            vec!["--offline", "flake", "lock"],
            "",
            true,
            1,
        ),
        // Locking for the evaluation uses the earlier download.
        (
            "evaluate, offline",
            vec!["--offline", "eval"],
            "#d",
            true,
            0,
        ),
        (
            "update, offline",
            vec!["--offline", "flake", "update", "--flake"],
            "",
            false,
            1,
        ),
    ];
    // Each program locks a flake of its own; the messages name it as DIR.
    let flakes: Vec<PathBuf> = ["brix", "nix"]
        .iter()
        .map(|who| {
            let dir = env.path().join(format!("flake-{who}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("flake.nix"), &flake_nix).unwrap();
            dir
        })
        .collect();
    for (what, args, fragment, fresh, downloads) in steps {
        let mut outcomes = Vec::new();
        for (who, dir) in ["brix", "nix"].iter().zip(&flakes) {
            if fresh {
                match std::fs::remove_file(dir.join("flake.lock")) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => panic!("removing the lock file: {e}"),
                }
            }
            let flake_ref = format!("path:{}{fragment}", dir.display());
            let mut args = args.clone();
            args.push(flake_ref.as_str());
            let cmd = if *who == "brix" {
                env.brix(&args)
            } else {
                env.nix(&args)
            };
            let (mut out, requests) = server.run(connected(cmd));
            let dir_text = dir.display().to_string();
            out.stdout = out.stdout.replace(&dir_text, "DIR");
            out.stderr = out.stderr.replace(&dir_text, "DIR");
            outcomes.push((out, requests));
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "{what}: brix (left) and nix (right) differ"
        );
        assert_eq!(outcomes[1].1, downloads, "{what}: {}", outcomes[1].0.stderr);
        assert_eq!(outcomes[1].0.status, 0, "{what}: {}", outcomes[1].0.stderr);
        let lock = |dir: &Path| std::fs::read_to_string(dir.join("flake.lock")).unwrap();
        assert_eq!(
            lock(flakes[0].as_path()),
            lock(flakes[1].as_path()),
            "{what}: the lock files"
        );
    }
}

/// One case of [`no_internet_is_warned_about_as_nix_does`]: its name, the
/// arguments, an environment variable to set, and whether Nix warns.
type WarningCase = (
    &'static str,
    Vec<&'static str>,
    Option<(&'static str, &'static str)>,
    bool,
);

/// Without a network interface, both warn before running the command
/// and go on as with `--offline`; `--offline` itself, a lower-case proxy
/// variable (only those count, by a bug of Nix's), and a verbosity below
/// `warn` silence the warning.
#[test]
fn no_internet_is_warned_about_as_nix_does() {
    let env = Env::new("");
    let warning =
        "warning: you don't have Internet access; disabling some network-dependent features\n";
    let cases: [WarningCase; 10] = [
        ("no network", vec!["eval", "--expr", "1"], None, true),
        (
            "--offline",
            vec!["--offline", "eval", "--expr", "1"],
            None,
            false,
        ),
        (
            "--no-net",
            vec!["--no-net", "eval", "--expr", "1"],
            None,
            false,
        ),
        (
            "two -q: warn",
            vec!["--quiet", "--quiet", "eval", "--expr", "1"],
            None,
            true,
        ),
        (
            "three -q: error",
            vec!["--quiet", "--quiet", "--quiet", "eval", "--expr", "1"],
            None,
            false,
        ),
        (
            "a failing command",
            vec!["eval", "--expr", "throw \"x\""],
            None,
            true,
        ),
        (
            "http_proxy",
            vec!["eval", "--expr", "1"],
            Some(("http_proxy", "")),
            false,
        ),
        (
            "all_proxy",
            vec!["eval", "--expr", "1"],
            Some(("all_proxy", "x")),
            false,
        ),
        (
            "HTTP_PROXY",
            vec!["eval", "--expr", "1"],
            Some(("HTTP_PROXY", "x")),
            true,
        ),
        (
            "no_proxy",
            vec!["eval", "--expr", "1"],
            Some(("no_proxy", "x")),
            true,
        ),
    ];
    for (what, args, var, warns) in cases {
        let with_var = |mut cmd: Command| {
            if let Some((k, v)) = var {
                cmd.env(k, v);
            }
            without_network(&cmd)
        };
        let ours = run(with_var(env.brix(&args)));
        let theirs = run(with_var(env.nix(&args)));
        assert_eq!(ours, theirs, "{what}: brix (left) and nix (right) differ");
        assert_eq!(
            theirs.stderr.starts_with(warning),
            warns,
            "{what}: {}",
            theirs.stderr
        );
    }
    // Nothing is checked for `--version`.
    let ours = run(without_network(&env.brix(&["--version"])));
    let theirs = run(without_network(&env.nix(&["--version"])));
    assert_eq!(
        (ours.status, ours.stderr.as_str()),
        (theirs.status, theirs.stderr.as_str())
    );
    assert_eq!(theirs.stderr, "");
}

/// `BRIX_HAVE_INTERNET` decides in place of the check: `0` gives Nix's
/// warning on a connected machine, `1` none without a network, and any
/// other value is an error.
#[test]
fn the_internet_check_can_be_forced() {
    let env = Env::new("");
    let args = ["eval", "--expr", "1"];
    let theirs = run(without_network(&env.nix(&args)));
    let mut forced_off = connected(env.brix(&args));
    forced_off.env("BRIX_HAVE_INTERNET", "0");
    assert_eq!(run(forced_off), theirs, "BRIX_HAVE_INTERNET=0");

    let mut forced_on = env.brix(&args);
    forced_on.env("BRIX_HAVE_INTERNET", "1");
    assert_eq!(
        run(without_network(&forced_on)),
        Output {
            status: 0,
            stdout: "1\n".to_owned(),
            stderr: String::new(),
        },
        "BRIX_HAVE_INTERNET=1"
    );

    let mut bad = env.brix(&args);
    bad.env("BRIX_HAVE_INTERNET", "maybe");
    assert_eq!(
        run(bad),
        Output {
            status: 1,
            stdout: String::new(),
            stderr: "error: BRIX_HAVE_INTERNET must be 1 or 0, not 'maybe'\n".to_owned(),
        }
    );
}

/// Once evaluation needs one input of a flake, brix fetches the others in
/// the background, where Nix fetches each only when it is needed. What a
/// background fetch says ("unpacking '…' into the Git cache...", a failed
/// download's retries) waits until evaluation needs the input: an input
/// that evaluation never needs says nothing, whether its download works or
/// fails, and one that it needs says what Nix says, after the same number
/// of attempts.
#[test]
fn inputs_fetched_ahead_say_nothing_until_needed() {
    let env = Env::new("");
    let server = Server::start(vec![
        ("a.tar.gz", flake_tarball(env.path(), "a")),
        ("b.tar.gz", flake_tarball(env.path(), "b")),
    ]);
    let flake = env.path().join("flake");
    std::fs::create_dir(&flake).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        format!(
            "{{ inputs.a.url = \"tarball+{}\"; inputs.b.url = \"tarball+{}\"; outputs = {{ self, a, b }}: {{ a = a.narHash; b = b.narHash; }}; }}\n",
            server.url("a.tar.gz"),
            server.url("b.tar.gz")
        ),
    )
    .unwrap();
    // Locked by `nix`, with caches of its own.
    let flake_ref = format!("path:{}", flake.display());
    let mut lock = connected(env.nix(&["flake", "lock", flake_ref.as_str()]));
    lock.env("XDG_CACHE_HOME", env.path().join("cache-lock"));
    let locked = run(lock);
    assert_eq!(locked.status, 0, "{}", locked.stderr);
    // `b` downloaded from `to` from now on: the flake and its lock say
    // so, and the lock keeps `b`'s NAR hash.
    let mut b_url = server.url("b.tar.gz");
    let mut move_b = |to: &str| {
        let to = server.url(to);
        for file in ["flake.nix", "flake.lock"] {
            let path = flake.join(file);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains(&b_url), "{file} names {b_url}");
            std::fs::write(&path, text.replace(&b_url, &to)).unwrap();
        }
        b_url = to;
    };
    // `eval` of an output, with caches of the step's own (so that every
    // input is fetched): what brix and nix print, and how many requests
    // for `b` (as the flake now names it) each made.
    let step = |what: &str, output: &str, b: &str| -> [(Output, usize); 2] {
        let installable = format!("path:{}#{output}", flake.display());
        let args = [
            "--filetransfer-retry-delay",
            "1",
            "eval",
            installable.as_str(),
        ];
        let mut ours = connected(env.brix(&args));
        ours.env(
            "XDG_CACHE_HOME",
            env.path().join(format!("cache-{what}-brix")),
        );
        let mut theirs = connected(env.nix(&args));
        theirs.env(
            "XDG_CACHE_HOME",
            env.path().join(format!("cache-{what}-nix")),
        );
        [ours, theirs].map(|cmd| {
            let before = server.requests_for(b);
            let out = run(cmd);
            (out, server.requests_for(b) - before)
        })
    };

    let [(ours, _), (theirs, _)] = step("works", "a", "b.tar.gz");
    assert_eq!(ours, theirs, "`b` downloads but is never needed");
    assert!(theirs.stdout.starts_with("\"sha256-"), "{}", theirs.stdout);

    // A background fetch may still be trying `b` when brix exits, so
    // brix's requests for it are not counted here.
    move_b("fail-unneeded.tar.gz");
    let [(ours, _), (theirs, their_requests)] = step("unneeded", "a", "fail-unneeded.tar.gz");
    assert_eq!(ours, theirs, "`b` fails but is never needed");
    assert_eq!(their_requests, 0);

    move_b("fail-needed.tar.gz");
    let [(ours, our_requests), (theirs, their_requests)] =
        step("needed", "b", "fail-needed.tar.gz");
    let retries = |out: &Output| out.stderr.matches("; retrying in ").count();
    assert_eq!(
        (ours.status, our_requests, retries(&ours)),
        (theirs.status, their_requests, retries(&theirs)),
        "`b` fails and is needed: brix (left) and nix (right) differ\nbrix: {}\nnix: {}",
        ours.stderr,
        theirs.stderr
    );
    assert_eq!((theirs.status, their_requests), (1, 5));
}

/// The same for Git inputs, which Nix fetches by running `git fetch`
/// (bare repositories count as remote ones): what `git` prints ("remote:
/// Counting objects…" with its carriage returns, "From …") appears only
/// for the inputs evaluation needs, in the order evaluation needs them,
/// byte for byte as under Nix, though brix fetches the other input in the
/// background.
#[test]
fn git_inputs_fetched_ahead_say_nothing_until_needed() {
    let env = Env::new("");
    let mut urls = Vec::new();
    for name in ["a", "b"] {
        let repo = GitRepo::new();
        repo.write(
            "flake.nix",
            format!(
                "# {}\n{{ outputs = {{ self }}: {{ v = \"{name}\"; }}; }}\n",
                env.path().display()
            ),
        );
        repo.commit();
        let bare = env.path().join(format!("{name}.git"));
        git(
            env.path(),
            &[
                "clone",
                "-q",
                "--bare",
                repo.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        urls.push(format!("git+file://{}", bare.display()));
    }
    let flake = env.path().join("flake");
    std::fs::create_dir(&flake).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        format!(
            "{{ inputs.a.url = \"{}\"; inputs.b.url = \"{}\"; outputs = {{ self, a, b }}: {{ a = a.narHash; b = b.narHash; ab = builtins.seq a.narHash b.narHash; }}; }}\n",
            urls[0], urls[1]
        ),
    )
    .unwrap();
    // Git's own configuration, which decides what it prints, is the same
    // for both: none.
    let quiet_git = |mut cmd: Command| {
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    };
    let flake_ref = format!("path:{}", flake.display());
    let mut lock = quiet_git(connected(env.nix(&["flake", "lock", flake_ref.as_str()])));
    lock.env("XDG_CACHE_HOME", env.path().join("cache-lock"));
    let locked = run(lock);
    assert_eq!(locked.status, 0, "{}", locked.stderr);
    for output in ["a", "b", "ab"] {
        let installable = format!("{flake_ref}#{output}");
        let args = ["eval", installable.as_str()];
        let mut ours = quiet_git(connected(env.brix(&args)));
        ours.env(
            "XDG_CACHE_HOME",
            env.path().join(format!("cache-{output}-brix")),
        );
        let mut theirs = quiet_git(connected(env.nix(&args)));
        theirs.env(
            "XDG_CACHE_HOME",
            env.path().join(format!("cache-{output}-nix")),
        );
        let (ours, theirs) = (run(ours), run(theirs));
        assert_eq!(ours, theirs, "{output}: brix (left) and nix (right) differ");
        assert_eq!(theirs.status, 0, "{output}: {}", theirs.stderr);
        // Each input that evaluation needs is fetched with `git fetch`.
        assert_eq!(
            theirs.stderr.matches("From file://").count(),
            output.len(),
            "{output}: {}",
            theirs.stderr
        );
    }
}
