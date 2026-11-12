//! Git inputs with submodules, `exportIgnore` and Nix 2.19 exports
//! (`nix-219-compat`) against the reference `nix`: what
//! `builtins.fetchGit` and `fetchTree` return, the files they show, flake
//! metadata (fingerprints included), lock files, warnings and errors.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it) and `git`. A
//! run of either program starts from a fetch cache of its own that is
//! empty, unless a test takes both through the same steps with a cache
//! each (both remember a tree's NAR hash under a fingerprint that, for a
//! working tree, does not say whether submodules were fetched), and the
//! reference `nix` writes into a chroot store in the test's directory,
//! brix into none.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{git, is_noise, GitRepo, Oracle, ORACLE_NIX_CONFIG};

/// The configuration both programs get on top of the oracle's pinned
/// one: no global flake registry (it would be downloaded) and no
/// substituters (a chroot store would ask them).
const EXTRA_CONFIG: &str = "flake-registry =\nsubstituters =";

/// What a run printed that the comparison looks at: the exit status,
/// stdout, and stderr (without the environment's noise; with what the
/// `git` both run prints), errors with their traces; and, for a failure,
/// its last error line, which tests check.
#[derive(Debug, PartialEq)]
struct Outcome {
    status: i32,
    stdout: String,
    stderr: Option<String>,
    error: Option<String>,
}

impl Outcome {
    /// The warnings on stderr, without their continuation lines.
    fn warnings(&self) -> Vec<&str> {
        self.stderr
            .iter()
            .flat_map(|s| s.lines())
            .filter(|l| l.starts_with("warning: "))
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
enum Side {
    Brix,
    Nix,
}

/// A test's directory: its repositories, and a fresh state (fetch cache,
/// and the reference `nix`'s store) for every run.
struct Scratch {
    dir: tempfile::TempDir,
    runs: Cell<u32>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            dir: tempfile::tempdir().unwrap(),
            runs: Cell::new(0),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A directory no run has used.
    fn fresh_state(&self) -> PathBuf {
        let n = self.runs.get() + 1;
        self.runs.set(n);
        let state = self.path().join(format!("state-{n}"));
        std::fs::create_dir(&state).unwrap();
        state
    }

    /// `args` on one side, in `cwd`, from a fresh state.
    fn run(&self, side: Side, cwd: &Path, args: &[&str]) -> Outcome {
        self.run_in(side, &self.fresh_state(), cwd, args)
    }

    /// `args` on one side, in `cwd`, with the fetch cache (and store) in
    /// `state`, which runs may share.
    fn run_in(&self, side: Side, state: &Path, cwd: &Path, args: &[&str]) -> Outcome {
        let (status, stdout, stderr) = match side {
            Side::Brix => {
                let out = Command::cargo_bin("brix")
                    .expect("brix binary")
                    .args(["--store", "dummy://"])
                    .args(args)
                    .current_dir(cwd)
                    .env("XDG_CACHE_HOME", state.join("cache"))
                    .env("NIX_CONFIG", format!("{ORACLE_NIX_CONFIG}\n{EXTRA_CONFIG}"))
                    .output()
                    .expect("run brix");
                (
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            }
            Side::Nix => {
                let out = Oracle::new()
                    .config(EXTRA_CONFIG)
                    .arg("--store")
                    .arg(state.join("store"))
                    .args(args)
                    .dir(cwd)
                    .env("XDG_CACHE_HOME", state.join("cache"))
                    .output();
                (out.code(), out.stdout_lossy(), out.stderr_lossy())
            }
        };
        let lines: Vec<&str> = stderr.lines().filter(|l| !is_noise(l.as_bytes())).collect();
        let error = match status {
            0 => None,
            _ => lines
                .iter()
                .map(|l| l.trim_start())
                .rfind(|l| l.starts_with("error: "))
                .map(str::to_owned),
        };
        Outcome {
            status,
            stdout,
            stderr: Some(lines.iter().map(|l| format!("{l}\n")).collect()),
            error,
        }
    }

    /// Both programs run `args` in `cwd` alike; `nix` succeeds.
    fn same(&self, cwd: &Path, args: &[&str]) -> Outcome {
        let theirs = self.run(Side::Nix, cwd, args);
        assert_eq!(theirs.status, 0, "nix failed for {args:?}: {theirs:?}");
        let ours = self.run(Side::Brix, cwd, args);
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ for {args:?}"
        );
        ours
    }

    /// Both programs fail alike for `args`.
    fn same_failure(&self, cwd: &Path, args: &[&str]) -> Outcome {
        let theirs = self.run(Side::Nix, cwd, args);
        assert_ne!(theirs.status, 0, "nix succeeded for {args:?}: {theirs:?}");
        let ours = self.run(Side::Brix, cwd, args);
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ for {args:?}"
        );
        ours
    }

    /// `flake lock` with `extra` in the flake at `dir` (no lock file
    /// yet) on both sides: the same lock file and output. The lock file
    /// is removed again; its text is returned.
    fn same_lock(&self, dir: &Path, extra: &[&str]) -> String {
        let lock_path = dir.join("flake.lock");
        let mut args = vec!["flake", "lock"];
        args.extend(extra);
        let take = |side| {
            let outcome = self.run(side, dir, &args);
            let lock = std::fs::read_to_string(&lock_path)
                .unwrap_or_else(|e| panic!("{side:?} wrote no lock file ({outcome:?}): {e}"));
            std::fs::remove_file(&lock_path).unwrap();
            (outcome, lock)
        };
        let theirs = take(Side::Nix);
        let ours = take(Side::Brix);
        assert_eq!(ours, theirs, "brix (left) and nix (right) differ");
        ours.1
    }
}

/// `sub`, a repository with one file, and `repo`, a checkout on `main`
/// with `flake.nix` holding `flake` and `sub` as its submodule `deps/sub`
/// by the relative URL `../sub`.
fn with_submodule(scratch: &Scratch, flake: &str) -> PathBuf {
    let sub = scratch.path().join("sub");
    let repo = scratch.path().join("repo");
    for d in [&sub, &repo] {
        std::fs::create_dir(d).unwrap();
        git(d, &["init", "-q", "-b", "main"]);
    }
    std::fs::write(sub.join("s.txt"), "in the submodule\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(&sub, &["commit", "-q", "-m", "s"]);
    std::fs::write(repo.join("flake.nix"), flake).unwrap();
    git(
        &repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "add",
            "../sub",
            "deps/sub",
        ],
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "top"]);
    repo
}

/// A flake showing its submodule's files and its `sourceInfo`.
const SUBMODULE_FLAKE: &str = "{ outputs = { self }: { files = builtins.readDir (self + \"/deps/sub\"); info = builtins.removeAttrs self.sourceInfo [ \"outPath\" ]; }; }\n";

/// `call`'s attributes (but `outPath`) and the files of `dir` in it.
fn tree_and(call: &str, dir: &str) -> String {
    format!(
        r#"let t = {call}; in {{ attrs = builtins.removeAttrs t [ "outPath" ]; files = builtins.readDir (t + "/{dir}"); }}"#
    )
}

/// `fetchGit` and `fetchTree` of a commit with and without `submodules`,
/// and a working tree (read as a flake, pure evaluation fetching only
/// locked inputs) with and without them, clean and with a dirty
/// submodule.
#[test]
fn fetched_submodules_match_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    let url = format!("file://{}", repo.display());
    let cwd = scratch.path();
    for call in [
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; submodules = true; }}"#),
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}"#),
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; submodules = true; }}"#
        ),
        format!(r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; }}"#),
    ] {
        scratch.same(
            cwd,
            &["eval", "--json", "--expr", &tree_and(&call, "deps/sub")],
        );
    }
    let flake = format!("git+{url}");
    for reference in [format!("{flake}?submodules=1"), flake.clone()] {
        for output in ["info", "files"] {
            scratch.same(cwd, &["eval", "--json", &format!("{reference}#{output}")]);
        }
    }

    // A change in the submodule makes the whole tree dirty.
    std::fs::write(repo.join("deps/sub/s.txt"), "changed\n").unwrap();
    let dirty = scratch.same(
        cwd,
        &["eval", "--json", &format!("{flake}?submodules=1#info")],
    );
    assert_eq!(dirty.warnings().len(), 2, "{dirty:?}");
    scratch.same(
        cwd,
        &["eval", "--json", &format!("{flake}?submodules=1#files")],
    );
}

/// A flake in a repository with a submodule: its metadata (`;s` in the
/// fingerprint of a commit with submodules mounted) and files, by
/// reference and with `inputs.self.submodules`, which refetches it.
#[test]
fn flakes_with_submodules_match_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    let flake = format!("git+file://{}", repo.display());
    let cwd = scratch.path();
    for reference in [
        format!("{flake}?submodules=1"),
        format!("{flake}?submodules=1&rev={rev}"),
        format!("{flake}?rev={rev}"),
        flake.clone(),
    ] {
        scratch.same(cwd, &["flake", "metadata", "--json", &reference]);
        scratch.same(cwd, &["flake", "metadata", &reference]);
        scratch.same(cwd, &["eval", "--json", &format!("{reference}#files")]);
    }

    std::fs::write(
        repo.join("flake.nix"),
        SUBMODULE_FLAKE.replace("{ outputs", "{ inputs.self.submodules = true; outputs"),
    )
    .unwrap();
    git(&repo, &["commit", "-q", "-am", "self.submodules"]);
    let files = scratch.same(cwd, &["eval", "--json", &format!("{flake}#files")]);
    assert_eq!(files.stdout, "{\"s.txt\":\"regular\"}\n");
    scratch.same(cwd, &["flake", "metadata", "--json", &flake]);
}

/// A flake with a `submodules = true` input: the lock file.
#[test]
fn locking_a_submodules_input_matches_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    let consumer = scratch.path().join("consumer");
    std::fs::create_dir(&consumer).unwrap();
    std::fs::write(
        consumer.join("flake.nix"),
        format!(
            r#"{{
  inputs.r.url = "git+file://{}?submodules=1";
  inputs.r.flake = false;
  outputs = {{ self, r }}: {{ files = builtins.readDir (r + "/deps/sub"); }};
}}
"#,
            repo.display()
        ),
    )
    .unwrap();
    let lock = scratch.same_lock(&consumer, &[]);
    assert!(lock.contains("\"submodules\": true"), "{lock}");
}

/// `exportIgnore` and `submodules` set together, and a relative
/// submodule of a repository fetched by its `file://` URL, which libgit2
/// resolves to an invalid URL: Nix's errors.
#[test]
fn submodule_errors_match_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    let url = format!("file://{}", repo.display());
    let cwd = scratch.path();
    let both = format!(
        r#"(builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; submodules = true; exportIgnore = true; }}).narHash"#
    );
    // A working tree, as a flake.
    let both_flake = format!("git+{url}?submodules=1&exportIgnore=1");
    for args in [
        vec!["eval", "--json", "--expr", both.as_str()],
        vec!["flake", "metadata", both_flake.as_str()],
    ] {
        let out = scratch.same_failure(cwd, &args);
        assert_eq!(
            out.error.as_deref(),
            Some("error: exportIgnore and submodules are not supported together yet")
        );
    }

    git(
        cwd,
        &["clone", "-q", "--bare", repo.to_str().unwrap(), "bare.git"],
    );
    let bare = format!("file://{}", cwd.join("bare.git").display());
    let out = scratch.same_failure(
        cwd,
        &[
            "eval",
            "--json",
            "--expr",
            &format!(
                r#"(builtins.fetchGit {{ url = "{bare}"; rev = "{rev}"; submodules = true; }}).narHash"#
            ),
        ],
    );
    assert!(
        out.error
            .as_deref()
            .is_some_and(|e| e.contains("has unexpected authority")),
        "{out:?}"
    );
}

/// The files of `call`'s tree, recursively, and its NAR hash.
fn listing(call: &str) -> String {
    format!(
        r#"let t = {call}; ls = d: builtins.mapAttrs (n: v: if v == "directory" then ls (d + "/${{n}}") else v) (builtins.readDir d); in {{ files = ls t.outPath; narHash = t.narHash; }}"#
    )
}

/// A repository whose `.gitattributes` exercise `export-ignore`: plain
/// names (at any depth), a pattern for directories only, a directory,
/// a path pattern, a value (which git does not count), an unset one, a
/// nested `.gitattributes`, and `export-subst`, which only `git archive`
/// applies.
fn export_ignore_repo() -> (GitRepo, String) {
    let repo = GitRepo::new();
    repo.write(
        ".gitattributes",
        "ignored.txt export-ignore\ndironly/ export-ignore\ndir2 export-ignore\nlogs/*.log export-ignore\nvalued.txt export-ignore=yes\nunset.txt -export-ignore\nsubst.txt export-subst\n",
    );
    for (file, text) in [
        ("keep.txt", "keep\n"),
        ("ignored.txt", "ignored\n"),
        ("sub/ignored.txt", "ignored below\n"),
        ("dironly/a", "a\n"),
        ("dir2/b", "b\n"),
        ("logs/x.log", "log\n"),
        ("logs/keep.txt", "kept log\n"),
        ("valued.txt", "valued\n"),
        ("unset.txt", "unset\n"),
        ("subst.txt", "Commit: $Format:%H$\n"),
        ("nested/.gitattributes", "deep.txt export-ignore\n"),
        ("nested/deep.txt", "deep\n"),
        ("nested/other.txt", "other\n"),
        (
            "flake.nix",
            "{ outputs = { self }: let ls = d: builtins.mapAttrs (n: v: if v == \"directory\" then ls (d + \"/${n}\") else v) (builtins.readDir d); in { s = builtins.readFile ./subst.txt; ignored = builtins.pathExists ./ignored.txt; files = ls ./.; narHash = self.narHash; }; }\n",
        ),
    ] {
        repo.write(file, text);
    }
    std::os::unix::fs::symlink("keep.txt", repo.path().join("link")).unwrap();
    let rev = repo.commit();
    (repo, rev)
}

/// `exportIgnore` (`fetchGit`'s default) in a commit of a checkout, of a
/// bare clone (where no path is a directory to a pattern ending in `/`),
/// and in a working tree (as a flake); reading a hidden file; and the
/// attributes of a checkout's working tree applying to its commits, as
/// libgit2 reads them.
#[test]
fn export_ignore_matches_nix() {
    let scratch = Scratch::new();
    let (repo, rev) = export_ignore_repo();
    let url = format!("file://{}", repo.path().display());
    let cwd = scratch.path();
    git(
        cwd,
        &[
            "clone",
            "-q",
            "--bare",
            repo.path().to_str().unwrap(),
            "bare.git",
        ],
    );
    let bare = format!("file://{}", cwd.join("bare.git").display());
    let eval = |expr: &str| scratch.same(cwd, &["eval", "--json", "--expr", expr]);
    for reference in [format!("git+{url}?exportIgnore=1"), format!("git+{url}")] {
        for output in ["files", "narHash"] {
            scratch.same(cwd, &["eval", "--json", &format!("{reference}#{output}")]);
        }
    }
    for call in [
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}"#),
        format!(r#"builtins.fetchGit {{ url = "{bare}"; rev = "{rev}"; }}"#),
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; exportIgnore = true; }}"#
        ),
        format!(r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; }}"#),
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; exportIgnore = false; }}"#),
    ] {
        eval(&listing(&call));
    }
    let hidden = scratch.same_failure(
        cwd,
        &[
            "eval",
            "--expr",
            &format!(
                r#"builtins.readFile ((builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}) + "/ignored.txt")"#
            ),
        ],
    );
    assert_eq!(
        hidden.error.as_deref(),
        Some(
            "error: '/ignored.txt' does not exist because it was fetched with exportIgnore enabled"
        )
    );

    // An uncommitted rule hides a committed file of any commit.
    std::fs::write(
        repo.path().join(".gitattributes"),
        format!(
            "{}keep.txt export-ignore\n",
            std::fs::read_to_string(repo.path().join(".gitattributes")).unwrap()
        ),
    )
    .unwrap();
    eval(&listing(&format!(
        r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}"#
    )));
}

/// `nix-219-compat`: a commit exported by `git archive`, announced, and
/// fingerprinted as `;e;legacy`; a working tree is read as ever.
#[test]
fn nix_219_compat_matches_nix() {
    let scratch = Scratch::new();
    let (repo, rev) = export_ignore_repo();
    let url = format!("file://{}", repo.path().display());
    let flake = format!("git+{url}");
    let cwd = scratch.path();
    // `command --option nix-219-compat true rest…`.
    let with_compat = |command: &[&str], rest: &[&str]| {
        let mut all = command.to_vec();
        all.extend(["--option", "nix-219-compat", "true"]);
        all.extend(rest);
        scratch.same(cwd, &all)
    };
    for call in [
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}"#),
        format!(r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; }}"#),
    ] {
        let out = with_compat(&["eval"], &["--json", "--expr", &listing(&call)]);
        assert_eq!(out.warnings().len(), 1, "{out:?}");
        let subst = with_compat(
            &["eval"],
            &[
                "--raw",
                "--expr",
                &format!("builtins.readFile (({call}) + \"/subst.txt\")"),
            ],
        );
        assert_eq!(subst.stdout, format!("Commit: {rev}\n"));
    }
    for reference in [format!("{flake}?rev={rev}"), flake.clone()] {
        with_compat(&["flake", "metadata"], &[reference.as_str()]);
        with_compat(&["flake", "metadata"], &["--json", &reference]);
        for output in ["s", "files", "narHash"] {
            with_compat(&["eval"], &["--json", &format!("{reference}#{output}")]);
        }
    }
}

/// A lock made with `nix-219-compat` holds the `narHash` of `git
/// archive`'s tree: without it, Nix tries that export after its own,
/// takes it, and says to update the hash; with it, the other way round.
#[test]
fn nar_hashes_of_the_other_export_match_nix() {
    let scratch = Scratch::new();
    let (repo, _) = export_ignore_repo();
    let consumer = scratch.path().join("consumer");
    std::fs::create_dir(&consumer).unwrap();
    std::fs::write(
        consumer.join("flake.nix"),
        format!(
            r#"{{
  inputs.r.url = "git+file://{}?ref=main";
  inputs.r.flake = false;
  outputs = {{ self, r }}: {{ s = builtins.readFile (r + "/subst.txt"); }};
}}
"#,
            repo.path().display()
        ),
    )
    .unwrap();
    let compat = ["--option", "nix-219-compat", "true"];
    let legacy_lock = scratch.same_lock(&consumer, &compat);
    let lock = scratch.same_lock(&consumer, &[]);
    assert_ne!(legacy_lock, lock);

    for (text, extra) in [(&legacy_lock, &[][..]), (&lock, &compat[..])] {
        std::fs::write(consumer.join("flake.lock"), text).unwrap();
        let mut args = vec!["eval"];
        args.extend(extra);
        args.extend(["--json", ".#s"]);
        let out = scratch.same(&consumer, &args);
        assert_eq!(out.warnings().len(), 1, "{out:?}");
        std::fs::remove_file(consumer.join("flake.lock")).unwrap();
    }
}

/// A flake showing its NAR hash and its submodule's files.
const HASH_FLAKE: &str = "{ outputs = { self }: { narHash = self.narHash; files = builtins.readDir (self + \"/deps/sub\"); }; }\n";

/// The steps `steps` taken by both programs, each with a fetch cache (and
/// store) of its own that persists from step to step: the same outcome at
/// every step, and `nix` succeeding at each. The outcomes, in order.
fn same_steps(scratch: &Scratch, steps: &[Vec<String>]) -> Vec<Outcome> {
    let take = |side| {
        let state = scratch.fresh_state();
        steps
            .iter()
            .map(|args| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                scratch.run_in(side, &state, scratch.path(), &args)
            })
            .collect::<Vec<_>>()
    };
    let theirs = take(Side::Nix);
    let ours = take(Side::Brix);
    for ((step, ours), theirs) in steps.iter().zip(&ours).zip(&theirs) {
        assert_eq!(theirs.status, 0, "nix failed at {step:?}: {theirs:?}");
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ at {step:?}"
        );
    }
    ours
}

/// Nix's `fetchToStore` remembers a tree's NAR hash under the tree's
/// fingerprint. A working tree's (`git:<rev>`) is the same with its
/// submodules mounted and without, and the same as its commit's without
/// them: whichever of these is hashed first gives the others its hash.
/// `flake metadata` hashes nothing, so it remembers nothing.
#[test]
fn remembered_nar_hashes_match_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, HASH_FLAKE);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    let flake = format!("git+file://{}", repo.display());
    let with = format!("{flake}?submodules=1");
    let commit = |submodules: &str| {
        format!(
            r#"(builtins.fetchTree {{ type = "git"; url = "file://{}"; rev = "{rev}";{submodules} }}).narHash"#,
            repo.display()
        )
    };
    let eval = |reference: &str| vec!["eval".to_owned(), "--json".to_owned(), reference.to_owned()];
    let expr = |e: String| {
        vec![
            "eval".to_owned(),
            "--json".to_owned(),
            "--expr".to_owned(),
            e,
        ]
    };

    // Without submodules first: everything gets the plain tree's hash.
    let plain_first = same_steps(
        &scratch,
        &[
            eval(&format!("{flake}#narHash")),
            eval(&format!("{with}#narHash")),
            eval(&format!("{with}#files")),
            expr(commit(" submodules = true;")),
            expr(commit("")),
        ],
    );
    assert_eq!(plain_first[0].stdout, plain_first[1].stdout);
    assert_eq!(plain_first[0].stdout, plain_first[4].stdout);
    // The commit with its submodules mounted is fingerprinted apart (`;s`).
    assert_ne!(plain_first[0].stdout, plain_first[3].stdout);
    assert_eq!(plain_first[2].stdout, "{\"s.txt\":\"regular\"}\n");

    // With submodules first: the plain tree and the commit get its hash.
    let submodules_first = same_steps(
        &scratch,
        &[
            eval(&format!("{with}#narHash")),
            eval(&format!("{flake}#narHash")),
            eval(&format!("{flake}#files")),
            expr(commit("")),
        ],
    );
    assert_eq!(submodules_first[0].stdout, submodules_first[1].stdout);
    assert_eq!(submodules_first[0].stdout, submodules_first[3].stdout);
    assert_eq!(submodules_first[2].stdout, "{}\n");

    // `flake metadata` hashes nothing: the plain tree gets its own hash.
    let metadata_first = same_steps(
        &scratch,
        &[
            vec!["flake".to_owned(), "metadata".to_owned(), with.clone()],
            eval(&format!("{flake}#narHash")),
        ],
    );
    assert_eq!(metadata_first[1].stdout, plain_first[0].stdout);
}

/// A relative submodule URL resolves against the remote the checkout
/// tracks, its URL rewritten by `url.<base>.insteadOf` first, as libgit2
/// rewrites it.
#[test]
fn submodule_urls_follow_insteadof_like_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/org/repo.git",
        ],
    );
    git(
        &repo,
        &[
            "config",
            &format!("url.{}/.insteadOf", scratch.path().display()),
            "https://example.invalid/org/",
        ],
    );
    let call = format!(
        r#"builtins.fetchGit {{ url = "file://{}"; rev = "{rev}"; submodules = true; }}"#,
        repo.display()
    );
    let out = scratch.same(
        scratch.path(),
        &["eval", "--json", "--expr", &tree_and(&call, "deps/sub")],
    );
    assert!(out.stdout.contains("\"s.txt\":\"regular\""), "{out:?}");
}

/// A submodule whose checkout is missing (cloned without
/// `--recurse-submodules`) is an empty directory, which Nix takes for a
/// repository to fetch from: `git` says it is none, twice in each of two
/// tries, and the fetch fails.
#[test]
fn missing_submodule_checkouts_fail_like_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(&scratch, SUBMODULE_FLAKE);
    git(
        scratch.path(),
        &["clone", "-q", repo.to_str().unwrap(), "clone"],
    );
    let clone = scratch.path().join("clone");
    let out = scratch.same_failure(
        scratch.path(),
        &[
            "eval",
            "--json",
            &format!("git+file://{}?submodules=1#files", clone.display()),
        ],
    );
    assert_eq!(out.warnings().len(), 4, "{out:?}");
    assert_eq!(
        out.error,
        Some(format!(
            "error: Failed to fetch git repository 'file://{}/deps/sub'",
            clone.display()
        ))
    );
}

/// Files in a submodule's working tree are named after the submodule's
/// input, `«git+file://…/sub?submodules=1»`, and their place on disk,
/// where the working tree's own files are named by theirs.
#[test]
fn submodule_files_are_named_like_nix() {
    let scratch = Scratch::new();
    let repo = with_submodule(
        &scratch,
        "{ outputs = { self }: { t = import (self + \"/deps/sub/bad.nix\"); }; }\n",
    );
    let sub = scratch.path().join("sub");
    std::fs::write(sub.join("bad.nix"), "throw \"boom\"\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(&sub, &["commit", "-q", "-m", "bad"]);
    let checkout = repo.join("deps/sub");
    git(&checkout, &["pull", "-q", sub.to_str().unwrap(), "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bad"]);
    let reference = format!("git+file://{}?submodules=1#t", repo.display());
    let out = scratch.same_failure(scratch.path(), &["eval", &reference]);
    assert_eq!(out.error.as_deref(), Some("error: boom"));
    let shown = format!(
        "«git+file://{}?submodules=1»{}/bad.nix",
        checkout.display(),
        checkout.display()
    );
    let stderr = out.stderr.unwrap_or_default();
    assert!(stderr.contains(&shown), "{stderr}");
}

/// An HTTP server on `127.0.0.1` that answers every request with a 500
/// and counts the requests; its URL.
fn failing_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            // The request line and headers, up to the empty line.
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            counted.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
                )
                .expect("answer the request");
        }
    });
    (url, count)
}

/// A remote Git repository whose server fails: without a `ref` Nix asks
/// it for its `HEAD` in each of its two tries, then fetches, three
/// requests in all (one with a `ref`). brix asks as often, and stderr is
/// the same: what the `git` both run says each time, the warnings about
/// the `HEAD` they could not read, the error.
#[test]
fn a_failing_git_server_is_asked_as_often_as_by_nix() {
    use std::sync::atomic::Ordering;
    let scratch = Scratch::new();
    let (url, count) = failing_server();
    let rev = "a".repeat(40);
    for (attrs, requests) in [("", 3), (r#" ref = "main";"#, 1)] {
        let expr = format!(
            r#"(builtins.fetchTree {{ type = "git"; url = "{url}/r"; rev = "{rev}";{attrs} }}).narHash"#
        );
        let args = ["eval", "--json", "--expr", &expr];
        // The server counts a request before it answers, so all of a
        // run's are counted when the run ends.
        let before = count.load(Ordering::SeqCst);
        let theirs = scratch.run(Side::Nix, scratch.path(), &args);
        let asked_by_nix = count.load(Ordering::SeqCst) - before;
        let ours = scratch.run(Side::Brix, scratch.path(), &args);
        let asked_by_brix = count.load(Ordering::SeqCst) - before - asked_by_nix;
        assert_ne!(theirs.status, 0, "nix succeeded: {theirs:?}");
        assert_eq!(asked_by_nix, requests, "{attrs}: {theirs:?}");
        assert_eq!(asked_by_brix, asked_by_nix, "{attrs}: {ours:?}");
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ for {attrs:?}"
        );
        assert_eq!(
            ours.error,
            Some(format!("error: Failed to fetch git repository '{url}/r'"))
        );
        let fatal =
            format!("fatal: unable to access '{url}/r/': The requested URL returned error: 500\n");
        let stderr = ours.stderr.as_deref().unwrap_or_default();
        assert_eq!(stderr.matches(&fatal).count(), requests, "{stderr}");
    }
}

/// A remote Git repository on a host that refuses connections fails as
/// in Nix, with what `git` says about each attempt. curl, which git uses,
/// says how long it tried to connect ("after 0 ms"), which is the one
/// thing that may differ.
#[test]
fn a_git_host_refusing_connections_fails_like_nix() {
    // Nothing listens on a privileged port of the loopback interface.
    let url = "http://127.0.0.1:1/r";
    let scratch = Scratch::new();
    let rev = "a".repeat(40);
    for attrs in ["", r#" ref = "main";"#] {
        let expr = format!(
            r#"(builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}";{attrs} }}).narHash"#
        );
        let args = ["eval", "--json", "--expr", &expr];
        let theirs = without_connect_times(scratch.run(Side::Nix, scratch.path(), &args));
        let ours = without_connect_times(scratch.run(Side::Brix, scratch.path(), &args));
        assert_ne!(theirs.status, 0, "nix succeeded: {theirs:?}");
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ for {attrs:?}"
        );
        assert_eq!(
            ours.error,
            Some(format!("error: Failed to fetch git repository '{url}'"))
        );
        let stderr = ours.stderr.as_deref().unwrap_or_default();
        let fatal = format!(
            "fatal: unable to access '{url}/': Failed to connect to 127.0.0.1:1 after N ms: Could not connect to server\n"
        );
        let attempts = if attrs.is_empty() { 3 } else { 1 };
        assert_eq!(stderr.matches(&fatal).count(), attempts, "{stderr}");
    }
}

/// A ref the clone of a remote has, fetched less than `tarball-ttl` ago,
/// is not fetched again (whichever input names it); fetched again when
/// the remote is gone, its failure is reported (git's messages, Nix's
/// error and warning) and the clone's version taken. Both programs keep
/// their own cache from step to step.
#[test]
fn a_failed_fetch_of_a_ref_the_clone_has_is_got_past_like_nix() {
    let scratch = Scratch::new();
    let work = scratch.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    let commit = |text: &str| {
        std::fs::write(work.join("flake.nix"), text).unwrap();
        git(&work, &["add", "flake.nix"]);
        git(&work, &["commit", "-q", "-m", "flake"]);
    };
    commit("{ outputs = _: { }; }\n");
    let bare = scratch.path().join("remote.git");
    git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let remote = format!("file://{}", bare.display());
    let by_head = format!("git+{remote}");
    let by_ref = format!("git+{remote}?ref=main");
    let nix_state = scratch.fresh_state();
    let brix_state = scratch.fresh_state();
    let same = |args: &[&str]| -> Outcome {
        let theirs = scratch.run_in(Side::Nix, &nix_state, scratch.path(), args);
        let ours = scratch.run_in(Side::Brix, &brix_state, scratch.path(), args);
        assert_eq!(theirs.status, 0, "nix failed for {args:?}: {theirs:?}");
        assert_eq!(
            ours, theirs,
            "brix (left) and nix (right) differ for {args:?}"
        );
        ours
    };
    let first = same(&["flake", "metadata", "--json", &by_head]);
    commit("{ outputs = _: { v = 2; }; }\n");
    git(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    let fresh = same(&["flake", "metadata", "--json", &by_ref]);
    let revision = |out: &Outcome| {
        let json: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        json["revision"].as_str().unwrap().to_owned()
    };
    assert_eq!(revision(&fresh), revision(&first));

    std::fs::remove_dir_all(&bare).unwrap();
    std::fs::create_dir(&bare).unwrap();
    let args = ["flake", "metadata", "--json", "--tarball-ttl", "0", &by_ref];
    let stale = same(&args);
    assert_eq!(revision(&stale), revision(&first));
    let stderr = stale.stderr.as_deref().unwrap_or_default();
    let reported = format!(
        "error: Failed to fetch git repository '{remote}'\nwarning: could not update local clone of Git repository '{remote}'; continuing with the most recent version\n"
    );
    assert!(stderr.ends_with(&reported), "{stderr}");
}

/// `outcome` with the times curl reports for a failed connection ("Failed
/// to connect to … after 3 ms") replaced by `N`.
fn without_connect_times(mut outcome: Outcome) -> Outcome {
    outcome.stderr = outcome.stderr.map(|stderr| {
        let mut out = String::new();
        let mut rest = stderr.as_str();
        while let Some(at) = rest.find(" after ") {
            let (head, tail) = rest.split_at(at + " after ".len());
            out.push_str(head);
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && tail[digits..].starts_with(" ms") {
                out.push('N');
                rest = &tail[digits..];
            } else {
                rest = tail;
            }
        }
        out.push_str(rest);
        out
    });
    outcome
}
