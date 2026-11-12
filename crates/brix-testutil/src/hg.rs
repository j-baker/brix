//! Mercurial repositories whose commits are the same on every run.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The date of every commit [`hg`] makes, as `hg commit --date` takes
/// it: 2024-01-02T03:04:05Z, [`crate::GIT_DATE`].
pub const HG_DATE: &str = "1704164645 0";

/// `hg` with no configuration file read (`HGRCPATH` empty), plain output
/// (`HGPLAIN`) and a fixed user, run in `dir`.
pub fn hg_command(dir: &Path) -> Command {
    let mut cmd = Command::new("hg");
    cmd.current_dir(dir)
        .env("HGRCPATH", "")
        .env("HGPLAIN", "")
        .env("HGUSER", "t <t@example.org>");
    cmd
}

/// Runs `hg <args>` in `dir` ([`hg_command`]) and returns its trimmed
/// stdout, failing the test (with hg's stderr) if it fails. Commits made
/// with [`HG_DATE`] are the same everywhere.
pub fn hg(dir: &Path, args: &[&str]) -> String {
    let out = hg_command(dir).args(args).output().unwrap_or_else(|e| {
        panic!("cannot run `hg` ({e}): it must be on PATH, as it is in `nix develop`")
    });
    assert!(
        out.status.success(),
        "hg {args:?} in {} failed:\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("hg printed UTF-8")
        .trim()
        .to_owned()
}

/// A Mercurial repository in `repo/` inside a temporary directory that
/// the test can put other things in (fetch caches, stores), all removed
/// on drop. Commands run through [`hg`].
pub struct HgRepo {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl HgRepo {
    /// An empty repository, on the branch `default`.
    pub fn new() -> HgRepo {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let path = dir.path().join("repo");
        hg(dir.path(), &["init", "repo"]);
        HgRepo { dir, path }
    }

    /// The repository's working tree.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The temporary directory the repository is in.
    pub fn scratch(&self) -> &Path {
        self.dir.path()
    }

    /// `hg <args>` in the repository; its trimmed stdout.
    pub fn hg(&self, args: &[&str]) -> String {
        hg(&self.path, args)
    }

    /// Writes `contents` to `rel` in the working tree, creating
    /// directories as needed.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.path.join(rel);
        let parent = path
            .parent()
            .expect("a file in the repository has a parent");
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
        std::fs::write(&path, contents)
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    }

    /// Commits every change in the working tree, new and deleted files
    /// included (`--addremove`), on the branch checked out; the new
    /// revision's node (40 hex digits).
    pub fn commit(&self) -> String {
        self.hg(&["commit", "-q", "--addremove", "-m", "c", "-d", HG_DATE]);
        self.hg(&["log", "-r", ".", "--template", "{node}"])
    }

    /// Starts the branch `name` with the next commit.
    pub fn branch(&self, name: &str) {
        self.hg(&["branch", "-q", name]);
    }
}

impl Default for HgRepo {
    fn default() -> HgRepo {
        HgRepo::new()
    }
}
