//! Directories for tests.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A throwaway chroot store for oracle runs that need to write
/// (`derivation`, `toFile`, `builtins.path`): `--store <dir>` keeps the
/// logical store directory `/nix/store` while writing under
/// `<dir>/nix/store`, so the paths Nix reports are the ones brix must
/// reproduce. Removed when the returned directory is dropped.
pub fn scratch_store() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("brix-oracle-store-")
        .tempdir()
        .expect("create a scratch store directory")
}

/// Copies the tree at `from` to `to` (created if missing): directories
/// recursively, regular files with their permissions, symlinks as
/// symlinks.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
    let entries =
        std::fs::read_dir(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("reading {}: {e}", from.display()));
        let (src, dest) = (entry.path(), to.join(entry.file_name()));
        let file_type = entry
            .file_type()
            .unwrap_or_else(|e| panic!("{}: {e}", src.display()));
        if file_type.is_dir() {
            copy_dir(&src, &dest);
        } else if file_type.is_symlink() {
            let target =
                std::fs::read_link(&src).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
            std::os::unix::fs::symlink(&target, &dest)
                .unwrap_or_else(|e| panic!("creating {}: {e}", dest.display()));
        } else {
            std::fs::copy(&src, &dest)
                .unwrap_or_else(|e| panic!("copying {} to {}: {e}", src.display(), dest.display()));
        }
    }
}

/// The file-system name of the bytes `name`, which need not be UTF-8
/// (`b"caf\xe9"`): for test trees with names Nix takes as bytes, and for
/// [`crate::Oracle::arg`] with an expression in bytes.
pub fn os(name: &[u8]) -> &OsStr {
    OsStr::from_bytes(name)
}

/// `dir` joined with the relative path `rel`, given as bytes.
pub fn join_bytes(dir: &Path, rel: &[u8]) -> PathBuf {
    dir.join(os(rel))
}

/// Writes `contents` to the file `rel` (bytes, which need not be UTF-8)
/// under `dir`, creating its directories as needed.
pub fn write_bytes(dir: &Path, rel: &[u8], contents: impl AsRef<[u8]>) {
    let path = join_bytes(dir, rel);
    let parent = path
        .parent()
        .expect("a file under a directory has a parent");
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
    std::fs::write(&path, contents).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// Makes `rel` under `dir` a symlink to `target`, both bytes that need
/// not be UTF-8, creating the link's directories as needed.
pub fn symlink_bytes(dir: &Path, rel: &[u8], target: &[u8]) {
    let path = join_bytes(dir, rel);
    let parent = path
        .parent()
        .expect("a link under a directory has a parent");
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
    std::os::unix::fs::symlink(os(target), &path)
        .unwrap_or_else(|e| panic!("creating {}: {e}", path.display()));
}
