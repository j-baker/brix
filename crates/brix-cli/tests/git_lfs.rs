//! Git inputs with `lfs = true` against the reference `nix`: commits whose
//! LFS pointers are smudged from a test LFS server (found from `origin`
//! over HTTP, or through `git-lfs-authenticate` run by a stand-in `ssh`),
//! the warnings about files that are no pointers, the errors of a failing
//! server, and the fingerprint of a working tree.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it) and `git`. Both
//! run with a `HOME` of their own (no Git configuration or attributes of
//! the user's) and a cache directory each.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use brix_store::{Hash, HashAlgo, HashFormat};
use brix_testutil::{GitRepo, LfsMode, LfsServer, Oracle};

/// Exit status, stdout and stderr of a run.
#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The `warning:` lines, without the reference `nix`'s environment
    /// noise. A warning that spans lines is its first.
    fn warnings(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|l| l.starts_with("warning:"))
            .collect()
    }

    /// The final error, from the last line that has `error:` to the end,
    /// each line trimmed (Nix indents a message's continuation lines).
    fn last_error(&self) -> Vec<&str> {
        let lines: Vec<&str> = self.stderr.lines().collect();
        let start = lines
            .iter()
            .rposition(|l| l.trim_start().starts_with("error:"))
            .unwrap_or_else(|| panic!("no error in:\n{}", self.stderr));
        lines[start..].iter().map(|l| l.trim()).collect()
    }
}

fn sha256(data: &[u8]) -> String {
    Hash::of(HashAlgo::Sha256, data).to_string(HashFormat::Base16, false)
}

const CONTENT: &[u8] = b"hello lfs\n";

fn pointer(data: &[u8]) -> String {
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        sha256(data),
        data.len()
    )
}

/// A repository whose commit has LFS pointers (`a.bin`, `sub/b.bin`), a
/// file `*.bin` marks that is no pointer and one too large to be one,
/// served by an LFS server whose URL `origin` gives.
struct Lfs {
    repo: GitRepo,
    rev: String,
    server: LfsServer,
}

impl Lfs {
    fn new(mode: LfsMode) -> Lfs {
        let server = LfsServer::start(vec![(sha256(CONTENT), CONTENT.to_vec())], mode);
        let repo = GitRepo::new();
        repo.write(
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        repo.write("a.bin", pointer(CONTENT));
        repo.write("sub/b.bin", pointer(CONTENT));
        repo.write("bad.bin", "not a pointer\n");
        repo.write("big.bin", "x".repeat(2000));
        repo.write("plain.txt", pointer(CONTENT));
        repo.write(
            "flake.nix",
            "{ outputs = { self }: { a = builtins.readFile ./a.bin; }; }\n",
        );
        let rev = repo.commit();
        repo.git(&[
            "remote",
            "add",
            "origin",
            &format!("{}/repo", server.base()),
        ]);
        Lfs { repo, rev, server }
    }

    fn url(&self) -> String {
        format!("file://{}", self.repo.path().display())
    }

    /// Removes both cache directories (fetched trees, LFS objects).
    fn clear_caches(&self) {
        for cache in ["cache-brix", "cache-nix"] {
            let dir = self.repo.scratch().join(cache);
            if dir.exists() {
                std::fs::remove_dir_all(dir).expect("remove a cache directory");
            }
        }
    }

    /// `args` run by brix and by `nix`, with `env` (and `PATH` led by
    /// `path`) for both.
    fn both(&self, args: &[&str], env: &[(&str, &str)], path: Option<&Path>) -> (Run, Run) {
        let scratch = self.repo.scratch();
        let home = scratch.join("home");
        std::fs::create_dir_all(&home).expect("create the home directory");
        let search_path = match path {
            Some(dir) => {
                let mut dirs = vec![dir.to_path_buf()];
                dirs.extend(std::env::split_paths(
                    &std::env::var_os("PATH").expect("PATH is set"),
                ));
                Some(std::env::join_paths(dirs).expect("a valid PATH"))
            }
            None => None,
        };
        let common: Vec<(String, PathBuf)> = vec![
            ("HOME".into(), home.clone()),
            ("XDG_CONFIG_HOME".into(), home.join(".config")),
        ];

        let mut ours = Command::cargo_bin("brix").expect("brix binary");
        ours.args(["--store", "dummy://"])
            .args(args)
            .env("XDG_CACHE_HOME", scratch.join("cache-brix"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let mut theirs = Oracle::new();
        theirs
            .args(args)
            .env("XDG_CACHE_HOME", scratch.join("cache-nix"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (key, value) in &common {
            ours.env(key, value);
            theirs.env(key, value);
        }
        for (key, value) in env {
            ours.env(key, value);
            theirs.env(key, value);
        }
        if let Some(search_path) = &search_path {
            ours.env("PATH", search_path);
            theirs.env("PATH", search_path);
        }
        let out = ours.output().expect("run brix");
        let ours = Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        let out = theirs.output();
        let theirs = Run {
            code: out.code(),
            stdout: out.stdout_lossy(),
            stderr: out.stderr_lossy(),
        };
        (ours, theirs)
    }

    /// The same output, warnings and status from both.
    fn assert_same(&self, what: &str, args: &[&str], env: &[(&str, &str)], path: Option<&Path>) {
        let (ours, theirs) = self.both(args, env, path);
        assert_eq!(ours.code, 0, "{what}: brix failed:\n{}", ours.stderr);
        assert_eq!(
            (ours.code, ours.stdout.as_str(), ours.warnings()),
            (theirs.code, theirs.stdout.as_str(), theirs.warnings()),
            "{what}: brix differs from nix\nbrix:\n{}\nnix:\n{}",
            ours.stderr,
            theirs.stderr
        );
    }

    /// The same failure from both.
    fn assert_same_error(&self, what: &str, args: &[&str]) {
        let (ours, theirs) = self.both(args, &[], None);
        assert_eq!(ours.code, 1, "{what}: brix:\n{}", ours.stderr);
        // The whole of stderr: the error with its traces ("… while
        // smudging git-lfs file '…'", "… while fetching the input '…'").
        assert_eq!(
            (ours.code, ours.stderr.as_str()),
            (theirs.code, theirs.stderr.as_str()),
            "{what}: brix differs from nix (final error: {:?})",
            theirs.last_error()
        );
    }
}

/// What both evaluate for a fetch of the commit: its `narHash` and the
/// contents of its smudged files. (Reading `bad.bin` is left out: Nix's
/// lazy trees read it again, and warn again, on each read.)
fn smudged(call: &str) -> String {
    format!(
        r#"let t = {call}; in {{ inherit (t) narHash rev; a = builtins.readFile (t.outPath + "/a.bin"); b = builtins.readFile (t.outPath + "/sub/b.bin"); plain = builtins.readFile (t.outPath + "/plain.txt"); }}"#
    )
}

#[test]
fn lfs_commits_match_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    let (url, rev) = (lfs.url(), &lfs.rev);
    for call in [
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{rev}"; lfs = true; }}"#
        ),
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; lfs = true; exportIgnore = false; }}"#
        ),
    ] {
        // Each call starts from empty caches: Nix's caches its NAR hash
        // by fingerprint, which both calls share, and would then read (and
        // warn about) nothing.
        lfs.clear_caches();
        lfs.assert_same(
            &call,
            &["eval", "--json", "--expr", &smudged(&call)],
            &[],
            None,
        );
    }
    // A batch request and a download per pointer, by brix and by nix, for
    // each call.
    assert_eq!(lfs.server.requests().len(), 2 * 2 * 4);
}

/// The batch API's errors, as `nix` reports them.
#[test]
fn lfs_server_failures_match_nix() {
    for mode in [
        LfsMode::Status(404, "{\"message\":\"not found\"}\n"),
        LfsMode::Status(200, "{\"foo\":1}"),
        LfsMode::ErrorObject,
        LfsMode::WrongSize,
        LfsMode::Corrupt,
        LfsMode::Short,
    ] {
        let lfs = Lfs::new(mode);
        let call = format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{}"; rev = "{}"; lfs = true; }}"#,
            lfs.url(),
            lfs.rev
        );
        lfs.assert_same_error(
            &format!("{mode:?}"),
            &["eval", "--json", "--expr", &format!("({call}).narHash")],
        );
    }
}

/// A checkout without `origin`: no server to ask, an error before any
/// file is read.
#[test]
fn lfs_without_a_remote_matches_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    lfs.repo.git(&["remote", "remove", "origin"]);
    let call = format!(
        r#"builtins.fetchTree {{ type = "git"; url = "{}"; rev = "{}"; lfs = true; }}"#,
        lfs.url(),
        lfs.rev
    );
    lfs.assert_same_error(
        "no origin",
        &["eval", "--expr", &format!("({call}).narHash")],
    );
}

/// An SSH `origin`: the endpoint and its `Authorization` come from
/// `ssh … git-lfs-authenticate <path> download`, run by a stand-in `ssh`
/// that records its arguments; a relative SCP-style path is warned
/// about.
#[test]
fn lfs_over_ssh_matches_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    let bin = lfs.repo.scratch().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let args_file = lfs.repo.scratch().join("ssh-args");
    let ssh = bin.join("ssh");
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '{{\"href\":\"{}/custom/lfs\",\"header\":{{\"Authorization\":\"{}\"}}}}'\n",
            args_file.display(),
            lfs.server.base(),
            "RemoteAuth xyz"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

    for (origin, expected_args) in [
        (
            "git@lfs.example:owner/repo.git",
            "-oFoo=bar\n-v\ngit@lfs.example\n--\ngit-lfs-authenticate\n/owner/repo.git\ndownload\n",
        ),
        (
            "ssh://git@lfs.example:2222/owner/repo/",
            "-oFoo=bar\n-v\n-p2222\ngit@lfs.example\n--\ngit-lfs-authenticate\n/owner/repo\ndownload\n",
        ),
    ] {
        lfs.repo.git(&["remote", "set-url", "origin", origin]);
        std::fs::write(&args_file, "").unwrap();
        // Each starts without LFS objects in its cache.
        lfs.clear_caches();
        let call = format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{}"; rev = "{}"; lfs = true; }}"#,
            lfs.url(),
            lfs.rev
        );
        let env = [("NIX_SSHOPTS", "-oFoo=bar -v")];
        let args = ["eval", "--json", "--expr", &smudged(&call)];
        let (ours, theirs) = lfs.both(&args, &env, Some(bin.as_path()));
        // `git-lfs-authenticate` for each of the two pointers, by each.
        let ssh_args = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(ssh_args, expected_args.repeat(4), "{origin}");
        assert_eq!(ours.code, 0, "{origin}: brix failed:\n{}", ours.stderr);
        // The SCP warning spans lines: all of stderr is compared.
        let ours_stderr =
            String::from_utf8_lossy(&brix_testutil::strip_noise(ours.stderr.as_bytes()))
                .into_owned();
        assert_eq!(
            (ours.code, ours.stdout.as_str(), ours_stderr.as_str()),
            (theirs.code, theirs.stdout.as_str(), theirs.stderr.as_str()),
            "{origin}: brix differs from nix"
        );
    }
    // The batch requests carried the `Authorization` git-lfs-authenticate
    // gave.
    let batches: Vec<_> = lfs
        .server
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST")
        .collect();
    assert!(!batches.is_empty());
    for batch in batches {
        assert_eq!(batch.target, "/custom/lfs/objects/batch");
        assert_eq!(batch.header("authorization"), Some("RemoteAuth xyz"));
    }
}

/// A working tree is read as it is, and its fingerprint (clean and with
/// uncommitted changes) says `;l`.
#[test]
fn lfs_working_trees_match_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    let flake = format!("git+{}?lfs=1", lfs.url());
    let fingerprint = |run: &Run| -> String {
        let json: serde_json::Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}\n{}", run.stdout, run.stderr));
        json["fingerprint"].to_string()
    };
    for state in ["clean", "dirty"] {
        if state == "dirty" {
            lfs.repo.write(
                "flake.nix",
                "{ outputs = { self }: { a = builtins.readFile ./a.bin; }; } # dirty\n",
            );
        }
        lfs.assert_same(state, &["eval", "--raw", &format!("{flake}#a")], &[], None);
        let (ours, theirs) = lfs.both(&["flake", "metadata", "--json", &flake], &[], None);
        assert_eq!(fingerprint(&ours), fingerprint(&theirs), "{state}");
    }
    assert!(lfs.server.requests().is_empty());
}

/// `inputs.self.lfs = true`: the flake's own tree is fetched again with
/// `lfs`, by its locked revision, so even a clean working tree's LFS files
/// are smudged.
#[test]
fn lfs_self_attribute_matches_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    lfs.repo.write(
        "flake.nix",
        "{ inputs.self.lfs = true; outputs = { self }: { a = builtins.readFile ./a.bin; }; }\n",
    );
    // Under lazy trees the flake's tree is not hashed: only `a.bin` is
    // read, so neither the files that are no pointers are warned about
    // nor is `sub/b.bin`'s object, which the server lacks, fetched.
    lfs.repo.write("sub/b.bin", pointer(b"not on the server\n"));
    let rev = lfs.repo.commit();
    let flake = format!("git+{}", lfs.url());
    for (what, flake) in [
        ("working tree", flake.clone()),
        ("revision", format!("{flake}?rev={rev}")),
    ] {
        lfs.clear_caches();
        lfs.assert_same(what, &["eval", "--raw", &format!("{flake}#a")], &[], None);
    }
    let missing = sha256(b"not on the server\n");
    assert!(
        lfs.server.requests().iter().all(|r| {
            !r.target.contains(&missing) && !String::from_utf8_lossy(&r.body).contains(&missing)
        }),
        "nobody asked for an object no file that was read needs"
    );
}

/// `fixGitURL`'s warning about an SCP-style URL with a relative path, for
/// any Git input (here one whose fetch then fails: `ssh` is a stand-in
/// that refuses): once, and exactly as `nix` words it.
#[test]
fn scp_style_git_urls_are_warned_about_like_nix() {
    let lfs = Lfs::new(LfsMode::Objects);
    let bin = lfs.repo.scratch().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\necho 'stand-in ssh refuses' >&2\nexit 255\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let expr = r#"builtins.fetchTree { type = "git"; url = "git@lfs.example:o/r.git"; rev = "0000000000000000000000000000000000000001"; narHash = "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="; }"#;
    let (ours, theirs) = lfs.both(
        &["eval", "--json", "--expr", expr],
        &[],
        Some(bin.as_path()),
    );
    let scp_warning = |run: &Run| -> (usize, String) {
        let start = run
            .stderr
            .find("warning: SCP-style URL")
            .unwrap_or_else(|| panic!("no SCP warning in:\n{}", run.stderr));
        let last = "    ssh://git@lfs.example/~/o/r.git";
        let end = start
            + run.stderr[start..]
                .find(last)
                .unwrap_or_else(|| panic!("an incomplete SCP warning in:\n{}", run.stderr))
            + last.len();
        (
            run.stderr.matches("warning: SCP-style URL").count(),
            run.stderr[start..end].to_owned(),
        )
    };
    assert_eq!(scp_warning(&ours), scp_warning(&theirs));
    assert_eq!(scp_warning(&ours).0, 1);
    // A `narHash` no store has: both fetch, and the stand-in ssh refuses.
    assert_eq!((ours.code, theirs.code), (1, 1), "{}", ours.stderr);
}
