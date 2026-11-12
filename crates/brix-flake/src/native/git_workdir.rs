//! A local Git working tree read in place: `GitRepo::getWorkdirInfo`
//! and the `AllowListSourceAccessor` that `getAccessorFromWorkdir` reads
//! it through (`src/libfetchers/git-utils.cc`, `git.cc`,
//! `filtering-source-accessor.cc`).
//!
//! When a `git+file:` input names a working tree and has neither a `ref`
//! nor a `rev`, Nix does not export `HEAD`: it reads the files from disk,
//! limited to the paths Git tracks. That is every path in the index
//! (staged additions included) minus the files deleted from the working
//! tree; untracked files are invisible, and reading one is an error that
//! says to `git add` it. The tree is *dirty* when any tracked path
//! differs between `HEAD`, the index and the working tree.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use brix_store::accessor::{AccessorError, DirEntries, SourceAccessor, Stat};
use brix_store::{BStr, BString, CanonPath, FsAccessor};

use crate::hashes::GitRev;
use crate::input::Input;
use crate::{err, Result};

/// `GitRepo::WorkdirInfo`, restricted to what brix uses.
#[derive(Debug)]
pub struct WorkdirInfo {
    /// `HEAD`'s commit; `None` on an unborn branch.
    pub head_rev: Option<GitRev>,
    /// Whether a tracked path differs between `HEAD`, the index and the
    /// working tree. Untracked files do not count.
    pub is_dirty: bool,
    /// The tracked paths that exist in the working tree.
    pub files: HashSet<CanonPath>,
    /// Those of `files` that differ between `HEAD`, the index and the
    /// working tree (`dirtyFiles`).
    pub dirty_files: HashSet<CanonPath>,
    /// The tracked paths deleted from the index or the working tree
    /// (`deletedFiles`).
    pub deleted_files: HashSet<CanonPath>,
    /// The gitlinks (submodules) in the index.
    pub submodules: Vec<CanonPath>,
}

impl WorkdirInfo {
    /// The revision a clean working tree is a checkout of, if it is clean
    /// and `HEAD` has a commit.
    pub fn clean_rev(&self) -> Option<GitRev> {
        if self.is_dirty {
            None
        } else {
            self.head_rev
        }
    }
}

/// A local checkout opened for reading in place.
pub struct Workdir {
    /// The directory the input's URL names (Nix's `locationToArg`).
    pub path: PathBuf,
    pub repo: gix::Repository,
    pub info: WorkdirInfo,
}

impl Workdir {
    pub fn open(path: &Path) -> Result<Workdir> {
        let repo = super::git::open_local(path)?;
        let info = workdir_info(&repo)?;
        Ok(Workdir {
            path: path.to_path_buf(),
            repo,
            info,
        })
    }
}

fn gix_err(context: &str, e: impl std::fmt::Display) -> crate::Error {
    err!("{}: {}", context, e)
}

/// A path Git gives (relative to the repository, its bytes unchecked, as
/// libgit2 gives them to Nix) as the tree's `CanonPath`.
pub(super) fn tracked_path(path: &gix::bstr::BStr) -> Result<CanonPath> {
    CanonPath::new(path).map_err(|e| err!("invalid Git path '{}': {}", path, e))
}

/// `repo` without the filter drivers its configuration defines
/// (`filter.<driver>.clean`/`process`, such as git-lfs's), as libgit2
/// sees it: it runs only its built-in filters (line endings, `ident`), so
/// a file that a driver's clean filter would change still matches the
/// index for Nix. gix would run the drivers to compare a file's contents
/// with the index.
fn without_filter_drivers(repo: &gix::Repository) -> Result<gix::Repository> {
    let mut repo = repo.clone();
    let mut config = repo.config_snapshot_mut();
    let drivers: Vec<_> = config
        .sections_and_ids_by_name("filter")
        .into_iter()
        .flatten()
        .map(|(_, id)| id)
        .collect();
    for id in drivers {
        config.remove_section_by_id(id);
    }
    config
        .commit()
        .map_err(|e| gix_err("configuring the working directory status", e))?;
    Ok(repo)
}

/// `GitRepoImpl::getWorkdirInfo`: libgit2's status with unmodified files
/// included, untracked files and submodules excluded and no rename
/// detection, and no filter drivers (see [`without_filter_drivers`]). A path counts as deleted when it is gone from the index
/// (`INDEX_DELETED`) or from the working tree (`WT_DELETED`); any other
/// status but "current" makes the tree dirty.
pub fn workdir_info(repo: &gix::Repository) -> Result<WorkdirInfo> {
    let head_rev = repo
        .head()
        .map_err(|e| gix_err("resolving HEAD", e))?
        .id()
        .map(|id| super::git::rev_from_oid(id.detach()))
        .transpose()?;

    let index = repo
        .index_or_empty()
        .map_err(|e| gix_err("opening the Git index", e))?;
    let mut files = HashSet::new();
    let mut submodules = Vec::new();
    for entry in index.entries() {
        let path = tracked_path(entry.path(&index))?;
        if entry.mode == gix::index::entry::Mode::COMMIT {
            if entry.stage() == gix::index::entry::Stage::Unconflicted {
                submodules.push(path);
            }
        } else {
            // A conflicted path has an entry per stage; it is one file.
            files.insert(path);
        }
    }

    use gix::status::index_worktree::Item as WorktreeItem;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    let status_repo = without_filter_drivers(repo)?;
    let status = status_repo
        .status(gix::progress::Discard)
        .map_err(|e| gix_err("getting working directory status", e))?
        .untracked_files(gix::status::UntrackedFiles::None)
        .index_worktree_submodules(None)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_options_mut(|opts| opts.dirwalk_options = None)
        .into_iter(Vec::new())
        .map_err(|e| gix_err("getting working directory status", e))?;
    let mut is_dirty = false;
    let mut changed = HashSet::new();
    let mut deleted_files = HashSet::new();
    for item in status {
        let item = item.map_err(|e| gix_err("getting working directory status", e))?;
        match item {
            gix::status::Item::TreeIndex(change) => {
                // Submodules are excluded from the status.
                if change.entry_mode() != gix::index::entry::Mode::COMMIT {
                    is_dirty = true;
                    let path = tracked_path(change.location())?;
                    if matches!(change, gix::diff::index::ChangeRef::Deletion { .. }) {
                        deleted_files.insert(path);
                    } else {
                        changed.insert(path);
                    }
                }
            }
            gix::status::Item::IndexWorktree(WorktreeItem::Modification {
                rela_path,
                status,
                ..
            }) => match status {
                // Only the cached stat information is out of date.
                EntryStatus::NeedsUpdate(_) => {}
                EntryStatus::Change(Change::SubmoduleModification(_)) => {}
                EntryStatus::Change(Change::Removed) => {
                    let path = tracked_path(rela_path.as_ref())?;
                    files.remove(&path);
                    deleted_files.insert(path);
                    is_dirty = true;
                }
                EntryStatus::Change(_)
                | EntryStatus::Conflict { .. }
                | EntryStatus::IntentToAdd => {
                    changed.insert(tracked_path(rela_path.as_ref())?);
                    is_dirty = true;
                }
            },
            // Untracked files are not walked (`UntrackedFiles::None`), and
            // would not count if they were.
            gix::status::Item::IndexWorktree(WorktreeItem::DirectoryContents { .. }) => {}
            gix::status::Item::IndexWorktree(WorktreeItem::Rewrite { .. }) => is_dirty = true,
        }
    }
    // A path deleted from the index or the working tree is deleted,
    // whatever else changed about it (libgit2 reports one status per path).
    let dirty_files = changed
        .into_iter()
        .filter(|p| !deleted_files.contains(p))
        .collect();

    Ok(WorkdirInfo {
        head_rev,
        is_dirty,
        files,
        dirty_files,
        deleted_files,
        submodules,
    })
}

/// `GitInputScheme::getFingerprint` for a working tree with uncommitted
/// changes (an input without `rev`): `git:<HEAD>;d=<hash>`, the hash
/// (SHA-512, in hexadecimal) covering the NAR of each changed file and
/// the name of each deleted one, in Nix's path order, with the markers of
/// `input`'s options (`;e`, `;l`, `;legacy`) before `;d=` as
/// `makeFingerprint` puts them. `None` when the tree has submodules, as
/// in Nix.
pub fn dirty_fingerprint(workdir: &Workdir, input: &Input) -> Result<Option<String>> {
    let info = &workdir.info;
    if !info.submodules.is_empty() {
        return Ok(None);
    }
    let nar_err = |e: brix_store::nar::NarError| err!("{}", e);
    let io_err = |e: std::io::Error| err!("hashing the working tree: {}", e);
    let fs = FsAccessor::new(&workdir.path);
    let mut hasher = brix_store::hash::Hasher::new(brix_store::HashAlgo::Sha512);
    let mut dirty: Vec<&CanonPath> = info.dirty_files.iter().collect();
    dirty.sort_by(|a, b| crate::fingerprint::nix_path_order(a, b));
    for file in dirty {
        brix_store::nar::write_str(&mut hasher, b"modified:").map_err(io_err)?;
        brix_store::nar::write_str(&mut hasher, file.as_bytes()).map_err(io_err)?;
        brix_store::nar::dump(&fs, file, &mut hasher).map_err(nar_err)?;
    }
    let mut deleted: Vec<&CanonPath> = info.deleted_files.iter().collect();
    deleted.sort_by(|a, b| crate::fingerprint::nix_path_order(a, b));
    for file in deleted {
        brix_store::nar::write_str(&mut hasher, b"deleted:").map_err(io_err)?;
        brix_store::nar::write_str(&mut hasher, file.as_bytes()).map_err(io_err)?;
    }
    // `options.makeFingerprint(headRev)`: the input's fingerprint at HEAD.
    let head = info.head_rev.unwrap_or(GitRev::NULL);
    let mut at_head = input.clone();
    at_head
        .attrs
        .insert("rev".into(), crate::attrs::Attr::String(head.to_hex()));
    let prefix = at_head
        .fingerprint()?
        .expect("invariant: a Git input with a rev has a fingerprint");
    Ok(Some(format!(
        "{prefix};d={}",
        crate::fingerprint::hex(&hasher.finish().0)
    )))
}

/// The working tree as Nix's `AllowListSourceAccessor` shows it: a path
/// is visible when it is tracked, is a directory on the way to a tracked
/// path, or lies below a tracked path (a tracked file that has become a
/// directory). Anything else does not exist, and reading it says why.
/// Paths are bytes, as Git and the file system have them.
///
/// A gitlink (submodule) is an empty directory, whatever is on disk:
/// `getAccessorFromWorkdir` mounts an empty accessor there when
/// submodules are not wanted.
pub struct WorkdirAccessor {
    fs: FsAccessor,
    repo: PathBuf,
    /// Tracked paths, in absolute form.
    files: HashSet<Vec<u8>>,
    /// The root and every ancestor directory of a tracked path.
    dirs: HashSet<Vec<u8>>,
    /// The gitlinks, in absolute form.
    empty: HashSet<Vec<u8>>,
    /// What a path is shown after, before its own place on disk: the
    /// input of a submodule's working tree (see [`Self::shown_as`]).
    shown_as: Option<String>,
}

/// Where a path is relative to the empty directories at the gitlinks.
enum Mount {
    /// At one of them.
    Root,
    /// Below one of them, where nothing exists.
    Inside,
    /// Neither: the working tree's file.
    Outside,
}

/// An absolute path without its last component (`/a/b` gives `/a`, `/a`
/// gives the empty string), or `None` once there is no `/` left.
fn cut_last(path: &[u8]) -> Option<&[u8]> {
    path.iter()
        .rposition(|&b| b == b'/')
        .map(|slash| &path[..slash])
}

impl WorkdirAccessor {
    pub fn new(
        repo: &Path,
        files: &HashSet<CanonPath>,
        submodules: &[CanonPath],
    ) -> WorkdirAccessor {
        let mut dirs = HashSet::new();
        dirs.insert(b"/".to_vec());
        // A mount point is visible like a tracked path (Nix adds it to the
        // allowed files).
        for file in files.iter().chain(submodules) {
            let mut path = file.as_bytes();
            while let Some(parent) = cut_last(path) {
                path = parent;
                if path.is_empty() || !dirs.insert(path.to_vec()) {
                    break;
                }
            }
        }
        // Nix's CanonPath::isAllowed checks only lower_bound's first
        // descendant candidate. With signed C chars, /a\xe9 sorts before
        // /a/b, so it can hide /a even though /a/b is tracked. Precompute
        // that visibility here, keeping ordinary lookups as hash probes.
        // ASCII paths (and unsigned-char platforms) have contiguous
        // descendants, so their ancestor set already gives Nix's answer.
        if (0xffu8 as std::ffi::c_char) < 0
            && files
                .iter()
                .chain(submodules)
                .any(|p| !p.as_bytes().is_ascii())
        {
            let mut allowed: Vec<_> = files.iter().chain(submodules).collect();
            allowed.sort_unstable_by(|a, b| crate::fingerprint::nix_path_order(a, b));
            dirs.retain(|bytes| {
                if bytes == b"/" {
                    return true;
                }
                let dir = CanonPath::new(bytes).expect("an ancestor is canonical");
                let next = allowed
                    .partition_point(|p| crate::fingerprint::nix_path_order(p, &dir).is_lt());
                allowed.get(next).is_some_and(|p| p.is_within(&dir))
            });
        }
        WorkdirAccessor {
            fs: FsAccessor::new(repo),
            repo: repo.to_path_buf(),
            files: files.iter().map(|f| f.as_bytes().to_vec()).collect(),
            dirs,
            empty: submodules.iter().map(|s| s.as_bytes().to_vec()).collect(),
            shown_as: None,
        }
    }

    /// The accessor with its paths shown after `prefix`, as Nix shows a
    /// submodule's working tree (`setPathDisplay("«<input>»")` on an
    /// accessor that shows its paths at their place on disk):
    /// `«git+file:///repo/sub?submodules=1»/repo/sub/file`.
    pub fn shown_as(mut self, prefix: String) -> WorkdirAccessor {
        self.shown_as = Some(prefix);
        self
    }

    fn mount(&self, path: &CanonPath) -> Mount {
        if self.empty.is_empty() {
            return Mount::Outside;
        }
        let mut p = path.as_bytes();
        if self.empty.contains(p) {
            return Mount::Root;
        }
        while let Some(parent) = cut_last(p) {
            p = parent;
            if self.empty.contains(p) {
                return Mount::Inside;
            }
        }
        Mount::Outside
    }

    fn not_found(&self, path: &CanonPath) -> AccessorError {
        AccessorError::NotFound(self.show_path(path))
    }

    /// `CanonPath::isAllowed` with the tracked paths as allowed prefixes
    /// and the root as the one allowed path.
    fn is_allowed(&self, path: &CanonPath) -> bool {
        let mut p = path.as_bytes();
        if self.dirs.contains(p) || self.files.contains(p) {
            return true;
        }
        while let Some(parent) = cut_last(p) {
            p = parent;
            if self.files.contains(p) {
                return true;
            }
        }
        false
    }

    /// `makeNotAllowedError`, its path and the repository's as the bytes
    /// they are (Nix formats both raw; `PathFmt` adds the double quotes).
    fn not_allowed(&self, path: &CanonPath) -> AccessorError {
        let rel: &[u8] = path.rel();
        let repo = self.repo.as_os_str().as_encoded_bytes();
        let exists = brix_store::os_path::to_path(rel)
            .is_some_and(|rel| std::fs::symlink_metadata(self.repo.join(rel)).is_ok());
        let message = if exists {
            [
                &b"Path '"[..],
                rel,
                b"' in the repository \"",
                repo,
                b"\" is not tracked by Git.\n\nTo make it visible to Nix, run:\n\ngit -C \"",
                repo,
                b"\" add -N \"",
                rel,
                b"\"",
            ]
            .concat()
        } else {
            [
                &b"Path '"[..],
                rel,
                b"' does not exist in Git repository \"",
                repo,
                b"\".",
            ]
            .concat()
        };
        AccessorError::Restricted(message.into())
    }

    fn check(&self, path: &CanonPath) -> std::result::Result<(), AccessorError> {
        if self.is_allowed(path) {
            Ok(())
        } else {
            Err(self.not_allowed(path))
        }
    }
}

const EMPTY_DIR: Stat = Stat {
    file_type: brix_store::FileType::Directory,
    size: None,
    executable: false,
};

impl SourceAccessor for WorkdirAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Ok(Some(EMPTY_DIR)),
            Mount::Inside => return Ok(None),
            Mount::Outside => {}
        }
        if !self.is_allowed(path) {
            return Ok(None);
        }
        self.fs.maybe_lstat(path)
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Ok(EMPTY_DIR),
            Mount::Inside => return Err(self.not_found(path)),
            Mount::Outside => {}
        }
        self.check(path)?;
        self.fs.lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Ok(DirEntries::new()),
            Mount::Inside => return Err(self.not_found(path)),
            Mount::Outside => {}
        }
        self.check(path)?;
        let mut entries = match self.fs.read_dir(path) {
            Ok(entries) => entries,
            // A directory that only holds gitlinks need not be on disk.
            Err(AccessorError::NotFound(_)) if self.dirs.contains(path.as_bytes()) => {
                DirEntries::new()
            }
            Err(e) => return Err(e),
        };
        entries.retain(|name, _| {
            path.join_component(name)
                .is_ok_and(|child| self.is_allowed(&child))
        });
        // The mount points are directories, on disk or not.
        for mount in &self.empty {
            let child = CanonPath::new(mount).expect("invariant: made from a CanonPath");
            if child.parent().as_ref() == Some(path) {
                let name = child.base_name().expect("invariant: not the root");
                entries.insert(name.to_owned(), Some(brix_store::FileType::Directory));
            }
        }
        Ok(entries)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Err(AccessorError::NotARegularFile(self.show_path(path))),
            Mount::Inside => return Err(self.not_found(path)),
            Mount::Outside => {}
        }
        self.check(path)?;
        self.fs.read_file(path)
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn Write,
    ) -> std::result::Result<u64, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Err(AccessorError::NotARegularFile(self.show_path(path))),
            Mount::Inside => return Err(self.not_found(path)),
            Mount::Outside => {}
        }
        self.check(path)?;
        self.fs.read_file_to(path, out)
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        match self.mount(path) {
            Mount::Root => return Err(AccessorError::NotASymlink(self.show_path(path))),
            Mount::Inside => return Err(self.not_found(path)),
            Mount::Outside => {}
        }
        self.check(path)?;
        self.fs.read_link(path)
    }

    fn display_prefix(&self) -> &BStr {
        self.fs.display_prefix()
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        match &self.shown_as {
            Some(prefix) => [prefix.as_bytes(), self.fs.show_path(path).as_slice()]
                .concat()
                .into(),
            None => self.fs.show_path(path),
        }
    }

    // No `physical_path`: the directory on disk also holds untracked
    // files, so it must not be read around this filter.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn accessor(dir: &Path, tracked: &[&str]) -> WorkdirAccessor {
        WorkdirAccessor::new(dir, &tracked.iter().map(p).collect(), &[])
    }

    #[test]
    fn only_tracked_paths_and_their_directories_are_visible() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        for f in ["a", "untracked", "sub/deep/b", "sub/c", "other/d"] {
            std::fs::write(dir.path().join(f), f).unwrap();
        }
        let acc = accessor(dir.path(), &["/a", "/sub/deep/b"]);

        let root: Vec<_> = acc
            .read_dir(&CanonPath::root())
            .unwrap()
            .into_keys()
            .collect();
        assert_eq!(root, ["a", "sub"]);
        let sub: Vec<_> = acc.read_dir(&p("/sub")).unwrap().into_keys().collect();
        assert_eq!(sub, ["deep"]);
        assert_eq!(acc.read_file(&p("/sub/deep/b")).unwrap(), b"sub/deep/b");
        assert!(acc.maybe_lstat(&p("/untracked")).unwrap().is_none());
        assert!(acc.maybe_lstat(&p("/other")).unwrap().is_none());
        assert!(acc.maybe_lstat(&p("/sub/c")).unwrap().is_none());
        assert!(acc.physical_path(&p("/a")).is_none());
    }

    #[test]
    fn a_tracked_path_that_became_a_directory_shows_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("was-file")).unwrap();
        std::fs::write(dir.path().join("was-file/x"), "x").unwrap();
        let acc = accessor(dir.path(), &["/was-file"]);
        assert_eq!(acc.read_file(&p("/was-file/x")).unwrap(), b"x");
    }

    #[test]
    fn hidden_paths_explain_themselves_like_nix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("new.nix"), "1").unwrap();
        let acc = accessor(dir.path(), &[]);
        let repo = dir.path().display();
        assert_eq!(
            acc.read_file(&p("/new.nix")).unwrap_err().to_string(),
            format!(
                "Path 'new.nix' in the repository \"{repo}\" is not tracked by Git.\n\nTo make it visible to Nix, run:\n\ngit -C \"{repo}\" add -N \"new.nix\""
            )
        );
        assert_eq!(
            acc.lstat(&p("/nope")).unwrap_err().to_string(),
            format!("Path 'nope' does not exist in Git repository \"{repo}\".")
        );
    }

    /// Tracked names are bytes, listed in unsigned byte order; reading an
    /// untracked one gives Nix's message with the name's bytes and the
    /// repository's.
    #[test]
    fn byte_names_are_tracked_and_explained_as_bytes() {
        let dir = tempfile::tempdir().unwrap();
        brix_testutil::write_bytes(dir.path(), b"caf\xe9", "1");
        brix_testutil::write_bytes(dir.path(), b"d\xe9/f", "2");
        brix_testutil::write_bytes(dir.path(), b"z", "3");
        brix_testutil::write_bytes(dir.path(), b"u\xe9", "u");
        let tracked: HashSet<CanonPath> = [p(b"/caf\xe9"), p(b"/d\xe9/f"), p(b"/z")]
            .into_iter()
            .collect();
        let acc = WorkdirAccessor::new(dir.path(), &tracked, &[]);
        let root: Vec<_> = acc
            .read_dir(&CanonPath::root())
            .unwrap()
            .into_keys()
            .collect();
        assert_eq!(root, [&b"caf\xe9"[..], b"d\xe9", b"z"]);
        assert_eq!(acc.read_file(&p(b"/d\xe9/f")).unwrap(), b"2");
        let repo = dir.path().as_os_str().as_encoded_bytes();
        assert_eq!(
            acc.read_file(&p(b"/u\xe9")).unwrap_err().message_bytes(),
            [
                &b"Path 'u\xe9' in the repository \""[..],
                repo,
                b"\" is not tracked by Git.\n\nTo make it visible to Nix, run:\n\ngit -C \"",
                repo,
                b"\" add -N \"u\xe9\"",
            ]
            .concat()
        );
        assert_eq!(
            acc.lstat(&p(b"/n\xe9")).unwrap_err().message_bytes(),
            [
                &b"Path 'n\xe9' does not exist in Git repository \""[..],
                repo,
                b"\".",
            ]
            .concat()
        );
    }
}
