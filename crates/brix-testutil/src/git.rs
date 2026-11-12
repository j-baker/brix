//! Git repositories whose commits are the same on every run.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The author and committer date of every commit [`git`] makes:
/// `lastModified` 1704164645.
pub const GIT_DATE: &str = "2024-01-02T03:04:05Z";

/// Runs `git <args>` in `dir` and returns its trimmed stdout, failing the
/// test (with git's stderr) if it fails. The user's and the system's Git
/// configuration are not read, signing is off, and the identity and dates
/// are fixed, so the same commands make the same commits everywhere.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.org")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.org")
        .env("GIT_AUTHOR_DATE", GIT_DATE)
        .env("GIT_COMMITTER_DATE", GIT_DATE)
        .output()
        .expect("the `git` command must be on PATH");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed:\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("git printed UTF-8")
        .trim()
        .to_owned()
}

/// A Git repository on branch `main`, in `repo/` inside a temporary
/// directory that the test can put other things in (fetch caches, a bare
/// clone), all removed on drop. Commands run through [`git`].
pub struct GitRepo {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl GitRepo {
    /// An empty repository.
    pub fn new() -> GitRepo {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let path = dir.path().join("repo");
        std::fs::create_dir(&path).expect("create the repository directory");
        let repo = GitRepo { dir, path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// The repository's working tree.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The temporary directory the repository is in.
    pub fn scratch(&self) -> &Path {
        self.dir.path()
    }

    /// `git <args>` in the repository; its trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        git(&self.path, args)
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

    /// Writes `contents` to `rel`, a relative path given as bytes that
    /// need not be UTF-8 (`b"d\xe9/caf\xe9"`), creating directories as
    /// needed.
    pub fn write_bytes(&self, rel: &[u8], contents: impl AsRef<[u8]>) {
        crate::fs::write_bytes(&self.path, rel, contents);
    }

    /// Makes `rel` a symlink to `target`, both bytes that need not be
    /// UTF-8, creating the link's directories as needed.
    pub fn symlink_bytes(&self, rel: &[u8], target: &[u8]) {
        crate::fs::symlink_bytes(&self.path, rel, target);
    }

    /// The working-tree path of `rel`, given as bytes.
    pub fn join_bytes(&self, rel: &[u8]) -> PathBuf {
        crate::fs::join_bytes(&self.path, rel)
    }

    /// Commits everything in the working tree; the new revision.
    pub fn commit(&self) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "c"]);
        self.git(&["rev-parse", "HEAD"])
    }
}

impl Default for GitRepo {
    fn default() -> GitRepo {
        GitRepo::new()
    }
}
