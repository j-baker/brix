//! The attributes of a fetched tree (`emitTreeAttrs`): what
//! `builtins.fetchGit` and `builtins.fetchTree` return and what a flake's
//! `sourceInfo` holds, for clean and dirty Git repositories, against the
//! reference `nix`.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it) and `git`.

use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{GitRepo, Oracle};

/// stdout, stderr and exit status of a command.
fn run(cmd: &mut Command) -> (i32, String, String) {
    let out = cmd.output().expect("run command");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The reference `nix` (brix-testutil's [`Oracle`]): stdout, stderr and
/// exit status.
fn nix(args: &[impl AsRef<std::ffi::OsStr>]) -> (i32, String, String) {
    let out = Oracle::new().args(args).output();
    (out.code(), out.stdout_lossy(), out.stderr_lossy())
}

/// A Git repository ([`GitRepo`]: fixed commit dates) with a brix fetch
/// cache of its own next to it.
struct Repo(GitRepo);

impl std::ops::Deref for Repo {
    type Target = GitRepo;
    fn deref(&self) -> &GitRepo {
        &self.0
    }
}

impl Repo {
    fn new() -> Repo {
        Repo(GitRepo::new())
    }

    /// `brix` and `nix` give the same output, messages and status for
    /// `args`, with `{repo}` standing for the repository's path.
    fn assert_matches_nix(&self, what: &str, args: &[&str]) {
        let repo = self.path().display().to_string();
        let args: Vec<String> = args.iter().map(|a| a.replace("{repo}", &repo)).collect();
        let ours = run(Command::cargo_bin("brix")
            .expect("brix binary")
            .args(["--store", "dummy://"])
            .args(&args)
            .env("XDG_CACHE_HOME", self.scratch().join("cache")));
        let theirs = nix(&args);
        assert_eq!(ours.0, 0, "{what}: brix failed:\n{}", ours.2);
        assert_eq!(
            (ours.0, ours.1.as_str(), warnings(&ours.2)),
            (theirs.0, theirs.1.as_str(), warnings(&theirs.2)),
            "{what}: brix differs from nix"
        );
    }
}

/// The warnings in `stderr`, without the reference `nix`'s environment
/// noise.
fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("warning:"))
        .collect()
}

fn fetch(builtin_call: &str) -> String {
    format!("builtins.removeAttrs ({builtin_call}) [ \"outPath\" ]")
}

/// A locked `fetchGit` and `fetchTree` of a clean repository: `rev`,
/// `shortRev`, `revCount`, `lastModified`, `lastModifiedDate`, `narHash`
/// and `submodules`. (`exportIgnore` and submodules are compared in
/// `git_inputs.rs`.)
#[test]
fn fetched_git_trees_match_nix() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.commit();
    repo.write("b", "b\n");
    let rev = repo.commit();
    let url = format!("file://{}", repo.path().display());
    for call in [
        format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; exportIgnore = false; }}"#),
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; exportIgnore = false; ref = "main"; }}"#
        ),
        format!(r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; }}"#),
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; shallow = true; }}"#
        ),
    ] {
        repo.assert_matches_nix(&call, &["eval", "--json", "--expr", &fetch(&call)]);
    }
}

/// A flake's `sourceInfo` (and `self`) in a clean and in a dirty working
/// tree: `dirtyRev` and `dirtyShortRev` instead of `rev` and `revCount`.
#[test]
fn flake_source_info_of_clean_and_dirty_trees_matches_nix() {
    let repo = Repo::new();
    repo.write(
        "flake.nix",
        r#"{
  outputs = { self }: {
    info = builtins.removeAttrs self.sourceInfo [ "outPath" ];
    shortRevs = [ (self.shortRev or null) (self.dirtyShortRev or null) ];
  };
}
"#,
    );
    repo.commit();
    let flake = |output: &str| format!("git+file://{{repo}}#{output}");
    repo.assert_matches_nix("clean", &["eval", "--json", &flake("info")]);
    repo.assert_matches_nix("clean", &["eval", "--json", &flake("shortRevs")]);
    repo.write(
        "flake.nix",
        std::fs::read_to_string(repo.path().join("flake.nix"))
            .unwrap()
            .replace("info =", "info  ="),
    );
    repo.assert_matches_nix("dirty", &["eval", "--json", &flake("info")]);
    repo.assert_matches_nix("dirty", &["eval", "--json", &flake("shortRevs")]);
}

/// A BLAKE3 `rev` or `narHash` is refused for the missing feature, and
/// with `blake3-hashes` enabled for its algorithm, with Nix's messages.
#[test]
fn blake3_revisions_and_nar_hashes_are_refused_like_nix() {
    let b3 = format!("blake3:{}", "a".repeat(64));
    let b3_sri = format!("blake3-{}=", "A".repeat(43));
    let last_error = |out: &(i32, String, String)| {
        out.2
            .lines()
            .rfind(|l| l.trim_start().starts_with("error:"))
            .map(str::to_owned)
    };
    for xp in [&[][..], &["--extra-experimental-features", "blake3-hashes"]] {
        for expr in [
            format!(
                r#"builtins.fetchTree {{ type = "github"; owner = "a"; repo = "b"; rev = "{b3}"; }}"#
            ),
            format!(r#"builtins.fetchTree {{ type = "git"; url = "https://x/y"; rev = "{b3}"; }}"#),
            format!(
                r#"builtins.fetchTree {{ type = "tarball"; url = "https://x/y.tar.gz"; narHash = "{b3_sri}"; }}"#
            ),
        ] {
            let ours = run(Command::cargo_bin("brix")
                .expect("brix binary")
                .args(["--store", "dummy://", "eval"])
                .args(xp)
                .args(["--expr", &expr]));
            let theirs = nix(&[&["eval"], xp, &["--expr", &expr]].concat());
            assert_eq!(ours.0, 1, "{expr} {xp:?}");
            assert_eq!(last_error(&ours), last_error(&theirs), "{expr} {xp:?}");
        }
    }
}
