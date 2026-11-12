//! Mercurial inputs and `builtins.fetchMercurial` against the reference
//! `nix` (`libfetchers/mercurial.cc`, `primops/fetchMercurial.cc`):
//! revisions, branches and working trees of local repositories and of a
//! repository `hg serve` serves; what `fetchMercurial` and `fetchTree`
//! return, lock files, flake metadata, and the errors.
//!
//! Every test needs `BRIX_NIX_BIN` and `hg` on `PATH` (the devshell has
//! both). Both programs run `hg` from `PATH` with no Mercurial
//! configuration (`HGRCPATH` empty) and no global flake registry, each
//! with a fetch cache of its own that starts empty: what a Mercurial
//! fetch reports depends on what that cache holds, so both are taken
//! through the same steps. The reference `nix` writes into a store of its
//! own (a chroot store in the test's directory), brix into none.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use brix_testutil::{hg_command, GitRepo, HgRepo, Oracle, ORACLE_NIX_CONFIG};

/// The configuration both programs get on top of the oracle's pinned
/// one: no global flake registry (it would be downloaded) and no
/// substituters (a chroot store would ask them for a tree it lacks).
const EXTRA_CONFIG: &str = "flake-registry =\nsubstituters =";

#[derive(Debug, PartialEq)]
struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

/// The two programs compared.
#[derive(Clone, Copy, Debug)]
enum Side {
    Brix,
    Nix,
}

/// Runs `args` on one side in `cwd`, with that side's fetch cache (and,
/// for `nix`, its store) in `scratch`. `path` replaces `PATH`.
fn run_with(side: Side, scratch: &Path, cwd: &Path, args: &[&str], path: Option<&str>) -> Outcome {
    match side {
        Side::Brix => {
            let mut cmd = Command::cargo_bin("brix").expect("brix binary");
            cmd.args(["--store", "dummy://"])
                .args(args)
                .current_dir(cwd)
                .env("XDG_CACHE_HOME", scratch.join("brix-cache"))
                .env("NIX_CONFIG", format!("{ORACLE_NIX_CONFIG}\n{EXTRA_CONFIG}"))
                .env("HGRCPATH", "");
            if let Some(path) = path {
                cmd.env("PATH", path);
            }
            let out = cmd.output().expect("run brix");
            Outcome {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
        }
        Side::Nix => {
            let mut oracle = Oracle::new();
            oracle
                .config(EXTRA_CONFIG)
                .arg("--store")
                .arg(scratch.join("nix-store"))
                .args(args)
                .dir(cwd)
                .env("XDG_CACHE_HOME", scratch.join("nix-cache"))
                .env("HGRCPATH", "");
            if let Some(path) = path {
                oracle.env("PATH", path);
            }
            let out = oracle.output();
            Outcome {
                status: out.code(),
                stdout: out.stdout_lossy(),
                stderr: out.stderr_lossy(),
            }
        }
    }
}

fn run(side: Side, scratch: &Path, cwd: &Path, args: &[&str]) -> Outcome {
    run_with(side, scratch, cwd, args, None)
}

/// Runs `args` with brix and then with nix, which must agree on the
/// status, the output and the messages; returns nix's outcome.
fn assert_same(what: &str, scratch: &Path, args: &[&str]) -> Outcome {
    let ours = run(Side::Brix, scratch, scratch, args);
    let theirs = run(Side::Nix, scratch, scratch, args);
    assert_eq!(ours, theirs, "{what}: brix differs from nix ({args:?})");
    theirs
}

/// [`assert_same`] for a command that must succeed.
fn assert_same_success(what: &str, scratch: &Path, args: &[&str]) -> Outcome {
    let theirs = assert_same(what, scratch, args);
    assert_eq!(theirs.status, 0, "{what}: nix failed:\n{}", theirs.stderr);
    theirs
}

/// An executable file and a symlink, which `hg archive` must reproduce.
fn add_special_files(repo: &HgRepo) {
    use std::os::unix::fs::PermissionsExt;
    repo.write("run.sh", "#!/bin/sh\n");
    let script = repo.path().join("run.sh");
    let mut mode = std::fs::metadata(&script).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&script, mode).unwrap();
    std::os::unix::fs::symlink("hello", repo.path().join("link")).unwrap();
}

/// Two commits on `default`, then one on `stable`; the three nodes.
fn three_commits(repo: &HgRepo) -> [String; 3] {
    repo.write("hello", "hello\n");
    repo.write("sub/x", "x\n");
    add_special_files(repo);
    let first = repo.commit();
    repo.write("hello", "world\n");
    let second = repo.commit();
    repo.branch("stable");
    repo.write("s", "s\n");
    let third = repo.commit();
    [first, second, third]
}

/// The attributes of a fetch, `outPath` (which `--json` would print
/// instead of the set) as `path`.
fn attrs_of(call: &str) -> String {
    format!("let r = {call}; in builtins.removeAttrs r [ \"outPath\" ] // {{ path = r.outPath; }}")
}

/// `fetchMercurial` and `fetchTree` of revisions: the first fetch of a
/// revision reports its branch, a later one (from the cache) `default`,
/// also within one evaluation; `rev` in upper case, a plain path as the
/// URL and a `name`; the archived tree with its executable file and
/// symlink.
#[test]
fn fetched_revisions_match_nix() {
    let repo = HgRepo::new();
    let [first, second, third] = three_commits(&repo);
    let scratch = repo.scratch();
    let path = repo.path().display().to_string();
    let url = format!("file://{path}");
    let upper = second.to_uppercase();
    for (what, call) in [
        (
            "a revision on a branch",
            format!(r#"builtins.fetchMercurial {{ url = "{url}"; rev = "{third}"; }}"#),
        ),
        (
            "the same revision again",
            format!(r#"builtins.fetchMercurial {{ url = "{url}"; rev = "{third}"; }}"#),
        ),
        (
            "a path, an upper-case revision and a name",
            format!(
                r#"builtins.fetchMercurial {{ url = "{path}"; rev = "{upper}"; name = "named"; }}"#
            ),
        ),
        (
            "fetchTree",
            format!(r#"builtins.fetchTree {{ type = "hg"; url = "{url}"; rev = "{first}"; }}"#),
        ),
    ] {
        assert_same_success(
            what,
            scratch,
            &["eval", "--json", "--expr", &attrs_of(&call)],
        );
    }
    let call =
        format!(r#"builtins.fetchMercurial {{ url = "{url}"; rev = "{third}"; name = "twice"; }}"#);
    let twice = format!("let f = {call}; in [ f.branch ({call}).branch ]");
    let out = assert_same_success(
        "twice in one evaluation",
        scratch,
        &["eval", "--json", "--expr", &twice],
    );
    assert_eq!(out.stdout, "[\"stable\",\"default\"]\n");
    let files = format!(
        r#"let r = builtins.fetchMercurial {{ url = "{url}"; rev = "{first}"; }}; in builtins.readDir r.outPath // {{ hello = builtins.readFile (r.outPath + "/hello"); link = builtins.readFileType (r.outPath + "/link"); }}"#
    );
    assert_same_success(
        "the archived files",
        scratch,
        &["eval", "--json", "--expr", &files],
    );
}

/// `fetchMercurial`'s own checks of its arguments, before anything is
/// fetched; in pure evaluation (brix's only mode) it needs a revision.
#[test]
fn fetch_mercurial_argument_errors_match_nix() {
    let repo = HgRepo::new();
    for expr in [
        r#"builtins.fetchMercurial { url = "file:///x"; foo = 1; }"#,
        r#"builtins.fetchMercurial { rev = "default"; }"#,
        r#"builtins.fetchMercurial { url = ""; }"#,
        r#"builtins.fetchMercurial "file:///x""#,
        r#"builtins.fetchMercurial { url = "file:///x"; rev = "default"; }"#,
        r#"builtins.fetchMercurial { url = 1; }"#,
        r#"builtins.fetchMercurial { url = "file:///x"; rev = 1; }"#,
    ] {
        let out = assert_same(expr, repo.scratch(), &["eval", "--expr", expr]);
        assert_eq!(out.status, 1, "{expr}: {}", out.stderr);
    }
}

/// A revision `hg` does not know, and no `hg` at all: `hg`'s complaint,
/// or the line Nix's child process prints (in colour) when it cannot
/// execute `hg`, then the error under "… while fetching the input '…'";
/// the fetch fails with the exit code 1 that child exits with. stderr is
/// compared whole.
#[test]
fn hg_failures_match_nix() {
    let repo = HgRepo::new();
    three_commits(&repo);
    let scratch = repo.scratch();
    let url = format!("file://{}", repo.path().display());
    let unknown = format!(
        r#"builtins.fetchMercurial {{ url = "{url}"; rev = "0123456789abcdef0123456789abcdef01234567"; }}"#
    );
    let args = ["eval", "--expr", unknown.as_str()];
    for path in [None, Some("/nonexistent")] {
        let ours = run_with(Side::Brix, scratch, scratch, &args, path);
        let theirs = run_with(Side::Nix, scratch, scratch, &args, path);
        assert_eq!(theirs.status, 1, "{path:?}: {}", theirs.stderr);
        assert_eq!(ours, theirs, "{path:?}: brix differs from nix");
        assert!(
            theirs
                .stderr
                .contains("… while fetching the input 'hg+file://"),
            "{}",
            theirs.stderr
        );
    }
    // A SHA-256 revision: an input like any other until the fetcher
    // refuses it.
    let sha256 = format!(
        r#"builtins.fetchTree {{ type = "hg"; url = "{url}"; rev = "sha256:{}"; }}"#,
        "a".repeat(64)
    );
    let out = assert_same("a SHA-256 revision", scratch, &["eval", "--expr", &sha256]);
    assert_eq!(out.status, 1, "{}", out.stderr);
}

/// The `narHash` a fetch reports, `expr` evaluating to the tree.
fn nar_hash_of(what: &str, scratch: &Path, expr: &str) -> String {
    let out = assert_same_success(
        what,
        scratch,
        &["eval", "--raw", "--expr", &format!("({expr}).narHash")],
    );
    out.stdout
}

/// Where Nix reports a fetched tree whose NAR hash is not the one asked
/// for, and what `fetchTree` returns for an input with one (a final
/// input: its own attributes). For a Mercurial or Git input, after the
/// fetch (without "… while fetching the input …"), ending with 102; for
/// a tarball, whose fetcher reports the hash, under that trace: a final
/// input's field mismatch from `fetchTree`, the hash mismatch (102) from
/// `fetchTarball`. A tarball that is not there fails under the trace too,
/// and an unlocked input checked by NAR hash is fetched after a warning.
#[test]
fn nar_hash_checks_match_nix() {
    let repo = HgRepo::new();
    let [first, ..] = three_commits(&repo);
    let scratch = repo.scratch();
    let wrong = "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";

    let url = format!("file://{}", repo.path().display());
    let hg = format!(r#"builtins.fetchTree {{ type = "hg"; url = "{url}"; rev = "{first}";"#);
    let right = nar_hash_of("a Mercurial revision", scratch, &format!("{hg} }}"));
    assert_same_success(
        "a Mercurial revision by its NAR hash",
        scratch,
        &[
            "eval",
            "--json",
            "--expr",
            &attrs_of(&format!(r#"{hg} narHash = "{right}"; }}"#)),
        ],
    );
    let out = assert_same(
        "a Mercurial revision with another NAR hash",
        scratch,
        &[
            "eval",
            "--expr",
            &format!(r#"{hg} narHash = "{wrong}"; }}"#),
        ],
    );
    assert_eq!(out.status, 102, "{}", out.stderr);

    // A Git working tree: unlocked, so checked by its NAR hash alone.
    let git_repo = GitRepo::new();
    git_repo.write("a", "a\n");
    let rev = git_repo.commit();
    let git_url = format!("file://{}", git_repo.path().display());
    let right = nar_hash_of(
        "a Git commit",
        scratch,
        &format!(r#"builtins.fetchTree {{ type = "git"; url = "{git_url}"; rev = "{rev}"; }}"#),
    );
    let workdir = |nar_hash: &str| {
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{git_url}"; narHash = "{nar_hash}"; }}"#
        )
    };
    assert_same_success(
        "a Git working tree by its NAR hash",
        scratch,
        &[
            "eval",
            "--json",
            "--expr",
            &format!("builtins.removeAttrs ({}) [ \"outPath\" ]", workdir(&right)),
        ],
    );
    let out = assert_same(
        "a Git working tree with another NAR hash",
        scratch,
        &["eval", "--expr", &workdir(wrong)],
    );
    assert_eq!(out.status, 102, "{}", out.stderr);

    // A tarball.
    let tarball = scratch.join("t.tar");
    let tar = Command::new("tar")
        .arg("-cf")
        .arg(&tarball)
        .arg("-C")
        .arg(repo.path())
        .args(["hello", "sub"])
        .status()
        .expect("`tar` must be on PATH");
    assert!(tar.success());
    let tarball_url = format!("file://{}", tarball.display());
    for (what, expr, status) in [
        (
            "fetchTree of a tarball with another NAR hash",
            format!(
                r#"builtins.fetchTree {{ type = "tarball"; url = "{tarball_url}"; narHash = "{wrong}"; }}"#
            ),
            1,
        ),
        (
            "fetchTarball with another hash",
            format!(r#"builtins.fetchTarball {{ url = "{tarball_url}"; sha256 = "{wrong}"; }}"#),
            102,
        ),
        (
            "a tarball that is not there",
            format!(
                r#"builtins.fetchTree {{ type = "tarball"; url = "{tarball_url}.gone"; narHash = "{wrong}"; }}"#
            ),
            1,
        ),
    ] {
        let out = assert_same(what, scratch, &["eval", "--expr", &expr]);
        assert_eq!(out.status, status, "{what}: {}", out.stderr);
    }
}

/// `brix flake …` and `nix flake …` steps on a flake in a Mercurial
/// working tree whose inputs are another one's default branch, a branch
/// and a revision: locking (the lock file, `hg add` of it), the metadata
/// of the then dirty tree, evaluation and `sourceInfo`. Each program
/// takes all the steps from the same start.
#[test]
fn hg_flakes_lock_and_evaluate_like_nix() {
    let dep = HgRepo::new();
    dep.write("flake.nix", "{ outputs = { self }: { x = 1; }; }\n");
    let first = dep.commit();
    dep.write("flake.nix", "{ outputs = { self }: { x = 2; }; }\n");
    dep.commit();
    dep.branch("stable");
    dep.write("flake.nix", "{ outputs = { self }: { x = 3; }; }\n");
    dep.commit();
    let dep_url = format!("hg+file://{}", dep.path().display());

    let top = HgRepo::new();
    top.write(
        "flake.nix",
        format!(
            r#"{{
  inputs.dep.url = "{dep_url}";
  inputs.stable.url = "{dep_url}?ref=stable";
  inputs.pinned.url = "{dep_url}?rev={first}";
  outputs = {{ self, dep, stable, pinned }}: {{
    xs = [ dep.x stable.x pinned.x ];
    info = builtins.removeAttrs self.sourceInfo [ "outPath" ];
    inputInfo = map (i: builtins.removeAttrs i.sourceInfo [ "outPath" ]) [ dep stable pinned ];
  }};
}}
"#
        ),
    );
    top.commit();
    let scratch = top.scratch();
    let flake = format!("hg+file://{}", top.path().display());
    let steps: [(&str, Vec<String>); 5] = [
        (
            "locking",
            vec![
                "flake".into(),
                "metadata".into(),
                "--json".into(),
                flake.clone(),
            ],
        ),
        (
            "dirty",
            vec![
                "flake".into(),
                "metadata".into(),
                "--json".into(),
                flake.clone(),
            ],
        ),
        (
            "inputs",
            vec!["eval".into(), "--json".into(), format!("{flake}#xs")],
        ),
        (
            "sourceInfo",
            vec!["eval".into(), "--json".into(), format!("{flake}#info")],
        ),
        (
            "inputs' sourceInfo",
            vec!["eval".into(), "--json".into(), format!("{flake}#inputInfo")],
        ),
    ];
    let lock_path = top.path().join("flake.lock");
    let take = |side: Side| -> (Vec<Outcome>, String, String) {
        let outcomes: Vec<Outcome> = steps
            .iter()
            .map(|(_, args)| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(side, scratch, scratch, &args)
            })
            .collect();
        let lock = std::fs::read_to_string(&lock_path)
            .unwrap_or_else(|e| panic!("{side:?} wrote no lock file: {e}"));
        let status = top.hg(&["status"]);
        // Back to the start: the lock file neither there nor added.
        top.hg(&["forget", "flake.lock"]);
        std::fs::remove_file(&lock_path).unwrap();
        (outcomes, lock, status)
    };
    let (ours, our_lock, our_status) = take(Side::Brix);
    let (theirs, their_lock, their_status) = take(Side::Nix);
    for (((what, args), ours), theirs) in steps.iter().zip(&ours).zip(&theirs) {
        assert_eq!(theirs.status, 0, "{what}: nix failed:\n{}", theirs.stderr);
        assert_eq!(ours, theirs, "{what}: brix differs from nix ({args:?})");
    }
    assert_eq!(our_lock, their_lock);
    assert_eq!(our_status, their_status);
    assert_eq!(their_status, "A flake.lock");
}

/// A working tree with uncommitted changes (modified, added, removed,
/// deleted and untracked files, an emptied directory): its tracked files
/// as they are on disk, the warning, `--no-warn-dirty` and
/// `allow-dirty = false`.
#[test]
fn dirty_working_trees_match_nix() {
    let repo = HgRepo::new();
    repo.write("a", "a\n");
    repo.write("b", "b\n");
    repo.write("c", "c\n");
    repo.write("s/t/x", "x\n");
    repo.write("s/y", "y\n");
    add_special_files(&repo);
    repo.commit();
    repo.write("a", "changed\n");
    repo.hg(&["rm", "-q", "b"]);
    std::fs::remove_file(repo.path().join("c")).unwrap();
    std::fs::remove_file(repo.path().join("s/t/x")).unwrap();
    repo.write("n/n", "n\n");
    repo.hg(&["add", "-q", "n/n"]);
    repo.write("untracked", "u\n");
    repo.write("ud/u", "u\n");
    let scratch = repo.scratch();
    let url = format!("hg+file://{}", repo.path().display());
    assert_same_success("dirty", scratch, &["flake", "prefetch", "--json", &url]);
    assert_same_success(
        "dirty, no warning",
        scratch,
        &["flake", "prefetch", "--json", "--no-warn-dirty", &url],
    );
    let refused = assert_same(
        "dirty, not allowed",
        scratch,
        &[
            "flake",
            "prefetch",
            "--json",
            "--option",
            "allow-dirty",
            "false",
            &url,
        ],
    );
    assert_eq!(refused.status, 1, "{}", refused.stderr);
    // A file that is merely missing from disk does not make a tree dirty:
    // the clean tree is cloned, with the file.
    repo.hg(&["revert", "-q", "--all", "--no-backup"]);
    std::fs::remove_file(repo.path().join("a")).unwrap();
    assert_same_success(
        "a file missing",
        scratch,
        &["flake", "prefetch", "--json", &url],
    );
}

/// `hg serve` of a repository on 127.0.0.1, stopped on drop.
struct HgServe {
    child: Child,
    url: String,
}

impl HgServe {
    fn start(repo: &HgRepo) -> HgServe {
        // A port nothing listens on now; `hg serve` takes it a moment later.
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let log = repo.scratch().join("hg-serve.log");
        let child = hg_command(repo.path())
            .args(["serve", "-a", "127.0.0.1", "-p", port.to_string().as_str()])
            .arg("-A")
            .arg(&log)
            .arg("-E")
            .arg(&log)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start `hg serve`");
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "`hg serve` did not start listening on {port}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        HgServe {
            child,
            url: format!("http://127.0.0.1:{port}"),
        }
    }
}

impl Drop for HgServe {
    fn drop(&mut self) {
        self.child.kill().expect("stop `hg serve`");
        self.child.wait().expect("wait for `hg serve`");
    }
}

/// A repository fetched over HTTP: cloned, a branch, the revision a
/// branch named reused for `tarball-ttl` after a new commit, and pulled
/// with `--refresh`; then a revision from the clone.
#[test]
fn served_repositories_match_nix() {
    let repo = HgRepo::new();
    three_commits(&repo);
    let server = HgServe::start(&repo);
    let scratch = repo.scratch();
    let url = format!("hg+{}", server.url);
    let stable = format!("{url}?ref=stable");
    assert_same_success("default", scratch, &["flake", "prefetch", "--json", &url]);
    assert_same_success(
        "a branch",
        scratch,
        &["flake", "prefetch", "--json", &stable],
    );
    repo.hg(&["update", "-q", "default"]);
    repo.write("hello", "again\n");
    let fourth = repo.commit();
    assert_same_success("cached", scratch, &["flake", "prefetch", "--json", &url]);
    let refreshed = assert_same_success(
        "refreshed",
        scratch,
        &["flake", "prefetch", "--json", "--refresh", &url],
    );
    assert!(refreshed.stdout.contains(&fourth), "{}", refreshed.stdout);
    let call = format!(
        r#"builtins.fetchTree {{ type = "hg"; url = "{}"; rev = "{fourth}"; }}"#,
        server.url
    );
    assert_same_success(
        "fetchTree",
        scratch,
        &["eval", "--json", "--expr", &attrs_of(&call)],
    );
}
