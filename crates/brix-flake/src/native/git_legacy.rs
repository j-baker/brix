//! Nix 2.19's export of a Git commit (`getLegacyGitAccessor` in
//! `src/libfetchers/git.cc`), which `nix-219-compat` turns on and which
//! Nix also falls back to when a locked `narHash` was made that way:
//! `git archive`, which applies `export-ignore`, `export-subst` and the
//! repository's filters, or, when submodules are wanted, `git checkout`
//! into a repository of its own (filters, but no export attributes; the
//! submodules are mounted on top). Like Nix, brix runs `git` for it.

use std::path::Path;
use std::process::{Command, Stdio};

use brix_store::{CanonPath, MemoryAccessor};

use crate::hashes::GitRev;
use crate::{err, Result};

/// The tree of `rev` in the repository at `repo_dir`, whose Git
/// directory is `git_dir` relative to it (`.git` for a checkout, `.` for
/// a bare clone), as Nix < 2.20 exported it. File names are bytes, as
/// Git has them; what libarchive warns about while unpacking the
/// archive (a long name in a pax header that is not ASCII) goes to
/// `warn`.
pub fn export(
    repo_dir: &Path,
    git_dir: &str,
    rev: &GitRev,
    submodules: bool,
    warn: &dyn Fn(&str),
) -> Result<MemoryAccessor> {
    if submodules {
        checkout(repo_dir, rev)
    } else {
        archive(repo_dir, git_dir, rev, warn)
    }
}

/// `git -C <repo> --git-dir <git dir> archive <rev>`, unpacked as it is
/// (`unpackTarfile`: no single top-level directory is stripped).
fn archive(
    repo_dir: &Path,
    git_dir: &str,
    rev: &GitRev,
    warn: &dyn Fn(&str),
) -> Result<MemoryAccessor> {
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(repo_dir)
        .args(["--git-dir", git_dir, "archive", &rev.to_hex()]);
    let tar = run(git)?;
    Ok(super::tarball::unpack_tar(&tar, warn)?.tree)
}

/// A fresh repository that fetches `rev` from `repo_dir` and checks it
/// out; its files without the `.git` directory.
fn checkout(repo_dir: &Path, rev: &GitRev) -> Result<MemoryAccessor> {
    let tmp = tempfile::Builder::new()
        .prefix("nix-")
        .tempdir()
        .map_err(|e| crate::Error::Io {
            context: "creating a temporary directory".into(),
            source: e,
        })?;
    let rev = rev.to_hex();
    let git = |args: &[&std::ffi::OsStr]| {
        let mut cmd = Command::new("git");
        cmd.args(args);
        cmd
    };
    let dir = tmp.path().as_os_str();
    run(git(&[
        "init".as_ref(),
        dir,
        "-b".as_ref(),
        "master".as_ref(),
    ]))?;
    run(git(&[
        "-C".as_ref(),
        dir,
        "remote".as_ref(),
        "add".as_ref(),
        "origin".as_ref(),
        repo_dir.as_os_str(),
    ]))?;
    run(git(&[
        "-C".as_ref(),
        dir,
        "fetch".as_ref(),
        "--quiet".as_ref(),
        "origin".as_ref(),
        rev.as_ref(),
    ]))?;
    run(git(&[
        "-C".as_ref(),
        dir,
        "checkout".as_ref(),
        "--quiet".as_ref(),
        rev.as_ref(),
    ]))?;
    let mut tree = MemoryAccessor::new();
    read_tree(&mut tree, tmp.path(), &CanonPath::root())?;
    Ok(tree)
}

/// `runProgram2` of a `git` command: its stdout, with stderr passed on;
/// a failure is Nix's `ExecError` ("program 'git' failed with exit code
/// 128").
fn run(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(super::stderr::for_program())
        .output()
        .map_err(|e| crate::Error::Io {
            context: "executing 'git'".into(),
            source: e,
        })?;
    super::stderr::pass_on(&out.stderr);
    if !out.status.success() {
        use std::os::unix::process::ExitStatusExt;
        // `statusToString`.
        let how = match (out.status.code(), out.status.signal()) {
            (Some(code), _) => format!("failed with exit code {code}"),
            (None, Some(signal)) => format!("was killed by signal {signal}"),
            (None, None) => "died abnormally".to_owned(),
        };
        return Err(err!("program 'git' {}", how));
    }
    Ok(out.stdout)
}

/// The files below `dir` into `tree` at `at`, leaving out everything
/// named `.git` (Nix's `PathFilter` for the checkout). Names and symlink
/// targets are taken as the bytes they are on disk.
fn read_tree(tree: &mut MemoryAccessor, dir: &Path, at: &CanonPath) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let io = |context: String| move |e| crate::Error::Io { context, source: e };
    tree.add_dir(at)?;
    let entries =
        std::fs::read_dir(dir).map_err(io(format!("reading directory '{}'", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(io(format!("reading directory '{}'", dir.display())))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let file = entry.path();
        let path = at
            .join_component(name.as_bytes())
            .map_err(|e| err!("invalid file name '{}': {}", file.display(), e))?;
        let meta = std::fs::symlink_metadata(&file)
            .map_err(io(format!("getting status of '{}'", file.display())))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&file)
                .map_err(io(format!("reading symlink '{}'", file.display())))?;
            tree.add_symlink(&path, target.as_os_str().as_bytes())?;
        } else if meta.is_dir() {
            read_tree(tree, &file, &path)?;
        } else if meta.is_file() {
            let contents =
                std::fs::read(&file).map_err(io(format!("reading '{}'", file.display())))?;
            tree.add_file(&path, contents, meta.permissions().mode() & 0o100 != 0)?;
        } else {
            return Err(err!("file '{}' has an unsupported type", file.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_store::SourceAccessor;
    use brix_testutil::GitRepo;

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn no_warnings(msg: &str) {
        panic!("unexpected warning: {msg}");
    }

    /// Names that are not UTF-8 come out of both exports as their bytes,
    /// symlink targets too. `git archive` puts a name too long for a tar
    /// header in a pax `path` record without `hdrcharset`, which
    /// libarchive cannot convert from UTF-8 to Nix's C locale: it keeps
    /// the bytes and warns (with U+FFFD for the byte, where Nix prints
    /// the byte itself: fetcher messages are text).
    #[test]
    fn exports_keep_names_as_bytes() {
        let repo = GitRepo::new();
        let mut long = b"d".to_vec();
        long.extend_from_slice(&[b'x'; 110]);
        long.push(0xe9);
        repo.write_bytes(b"caf\xe9", "1");
        repo.write_bytes(b"d\xe9/f", "2");
        repo.write_bytes(&long, "3");
        repo.symlink_bytes(b"l\xff", b"t\xe9");
        let rev = GitRev::parse(&repo.commit(), Default::default()).unwrap();

        let warnings = std::cell::RefCell::new(Vec::new());
        let warn = |msg: &str| warnings.borrow_mut().push(msg.to_owned());
        for submodules in [false, true] {
            warnings.borrow_mut().clear();
            let tree = export(repo.path(), ".git", &rev, submodules, &warn).unwrap();
            assert_eq!(tree.read_file(&p(b"/caf\xe9")).unwrap(), b"1");
            assert_eq!(tree.read_file(&p(b"/d\xe9/f")).unwrap(), b"2");
            let long_path = p([&b"/"[..], &long[..]].concat());
            assert_eq!(tree.read_file(&long_path).unwrap(), b"3");
            assert_eq!(tree.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9");
            let expected: &[String] = if submodules {
                &[]
            } else {
                &[format!(
                    "getting archive member '{}': Pathname can't be converted from UTF-8 to current locale",
                    String::from_utf8_lossy(&long)
                )]
            };
            assert_eq!(*warnings.borrow(), expected, "submodules: {submodules}");
        }
    }

    /// `git archive` applies `export-subst` and `export-ignore`; a
    /// checkout applies neither.
    #[test]
    fn archive_and_checkout_export_like_git() {
        let repo = GitRepo::new();
        repo.write(
            ".gitattributes",
            "subst.txt export-subst\nignored export-ignore\n",
        );
        repo.write("subst.txt", "Commit: $Format:%H$\n");
        repo.write("ignored", "i\n");
        repo.write("only/one", "1\n");
        let rev = GitRev::parse(&repo.commit(), Default::default()).unwrap();

        let archived = export(repo.path(), ".git", &rev, false, &no_warnings).unwrap();
        assert_eq!(
            archived.read_file(&p("/subst.txt")).unwrap(),
            format!("Commit: {}\n", rev.to_hex()).into_bytes()
        );
        assert!(!archived.path_exists(&p("/ignored")).unwrap());
        // A single directory at the top would still be kept.
        assert_eq!(archived.read_file(&p("/only/one")).unwrap(), b"1\n");

        let checked_out = export(repo.path(), ".git", &rev, true, &no_warnings).unwrap();
        assert_eq!(
            checked_out.read_file(&p("/subst.txt")).unwrap(),
            b"Commit: $Format:%H$\n"
        );
        assert!(checked_out.path_exists(&p("/ignored")).unwrap());
        assert!(!checked_out.path_exists(&p("/.git")).unwrap());

        let missing = GitRev::parse(&"1".repeat(40), Default::default()).unwrap();
        assert_eq!(
            export(repo.path(), ".git", &missing, false, &no_warnings)
                .unwrap_err()
                .to_string(),
            "program 'git' failed with exit code 128"
        );
    }
}
