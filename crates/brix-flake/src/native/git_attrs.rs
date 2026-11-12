//! Git attributes of the paths of a tree, as libgit2 finds them for Nix
//! (`git_attr_get_ext`): `export-ignore`, whose paths `exportIgnore`
//! leaves out (`GitExportIgnoreSourceAccessor` in
//! `src/libfetchers/git-utils.cc`), and `filter`, whose value `lfs` marks
//! a Git LFS pointer (`lfs::Fetch::shouldFetch`, see
//! [`super::git_lfs`]).
//!
//! brix reads the attribute files the way libgit2 does, with gix's
//! attribute parser and matcher:
//!
//! - `$GIT_DIR/info/attributes` comes first, then the `.gitattributes`
//!   of the path's directory and of each directory above it, nearest
//!   first, then the user's attributes file (`core.attributesFile`, by
//!   default `$XDG_CONFIG_HOME/git/attributes`). The system file is not
//!   read (`GIT_ATTR_CHECK_NO_SYSTEM`). The first file with a rule for
//!   the attribute that matches decides, the last such rule in it.
//! - A commit's paths (`GIT_ATTR_CHECK_INCLUDE_COMMIT`) look, at each
//!   directory, at the working tree's `.gitattributes`, then the
//!   index's, then the commit's: in a local checkout the attributes of
//!   the working tree apply to any commit exported from it. A bare
//!   repository (a clone of a remote) has only the commit's.
//! - A working tree's paths look at the index's files only
//!   (`GIT_ATTR_CHECK_INDEX_ONLY`).
//! - A file read from disk (the working tree's, `info/attributes`, the
//!   user's) that cannot be read, or is a directory, counts as missing,
//!   as libgit2 has it ("For open or read errors, pretend that we got
//!   ENOTFOUND"); and a file larger than `GIT_ATTR_MAX_FILE_SIZE`, from
//!   any source, is ignored.
//! - Macros are defined at the top only: in the root directory's
//!   `.gitattributes`, `info/attributes` and the user's file.
//! - Whether a path is a directory, which a pattern ending in `/` needs,
//!   is asked of the working tree on disk; in a bare repository nothing
//!   is.
//! - Only `export-ignore` set without a value hides a path (git ignores
//!   `export-ignore=…`), and a hidden directory is missing from its
//!   parent's listing while what is below it can still be read by path,
//!   as through Nix's filtering accessor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use brix_store::accessor::{AccessorError, DirEntries, SourceAccessor, Stat};
use brix_store::{os_path, BStr, BString, CanonPath, Hash};
use gix::attrs::search::{MetadataCollection, Outcome};
use gix::attrs::{Search, StateRef};
use gix::bstr::ByteSlice;
use gix::glob::pattern::Case;

use crate::{err, Result};

/// `GIT_ATTR_MAX_FILE_SIZE`: libgit2 ignores a larger attributes file.
const MAX_FILE_SIZE: usize = 100_000_000;

/// Attribute file contents keyed by their relative directory bytes.
pub type AttributeFiles = HashMap<Vec<u8>, Vec<u8>>;

/// Attribute files consulted for one kind of tree, keyed by directory
/// (`b""` for the root, `b"a/b"` below it).
pub struct AttrSources {
    /// The working tree of a non-bare repository: whether a path is a
    /// directory is asked of it.
    workdir: Option<PathBuf>,
    /// Whether the working tree's `.gitattributes` files count (for a
    /// commit; a working tree's own paths use the index's).
    read_workdir: bool,
    /// The `.gitattributes` files of the index.
    index: AttributeFiles,
    /// The `.gitattributes` files of the commit.
    commit: AttributeFiles,
    /// `$GIT_DIR/info/attributes`.
    info: Option<Vec<u8>>,
    /// The user's attributes file and its contents.
    global: Option<(PathBuf, Vec<u8>)>,
    /// `core.ignoreCase`.
    case: Case,
}

impl AttrSources {
    /// For the paths of a commit of `repo` whose `.gitattributes` files
    /// are `commit_files` (see [`commit_attr_file`]).
    pub fn for_commit(repo: &gix::Repository, commit_files: AttributeFiles) -> Result<AttrSources> {
        let workdir = repo.workdir().map(Path::to_path_buf);
        Ok(AttrSources {
            read_workdir: workdir.is_some(),
            workdir,
            index: index_attr_files(repo)?,
            commit: commit_files,
            info: info_attributes(repo),
            global: global_attributes(repo)?,
            case: case(repo),
        })
    }

    /// For the paths of `repo`'s working tree.
    pub fn for_workdir(repo: &gix::Repository) -> Result<AttrSources> {
        Ok(AttrSources {
            workdir: repo.workdir().map(Path::to_path_buf),
            read_workdir: false,
            index: index_attr_files(repo)?,
            commit: HashMap::new(),
            info: info_attributes(repo),
            global: global_attributes(repo)?,
            case: case(repo),
        })
    }

    /// The `.gitattributes` files of `dir`, lowest precedence first: the
    /// commit's, the index's, the working tree's.
    fn files_in(&self, dir: &[u8]) -> Vec<&[u8]> {
        let mut files = Vec::new();
        if let Some(bytes) = self.commit.get(dir) {
            files.push(bytes.as_slice());
        }
        if let Some(bytes) = self.index.get(dir) {
            files.push(bytes.as_slice());
        }
        files.retain(|bytes| bytes.len() <= MAX_FILE_SIZE);
        files
    }

    /// The working tree's `.gitattributes` of `dir`, if it counts and
    /// can be read (`GIT_ATTR_FILE_SOURCE_FILE`).
    fn workdir_file_in(&self, dir: &[u8]) -> Option<Vec<u8>> {
        match (self.read_workdir, &self.workdir) {
            (true, Some(workdir)) => {
                read_attr_file(&workdir.join(os_path::to_path(dir)?).join(".gitattributes"))
            }
            _ => None,
        }
    }

    /// `git_attr_path__init`'s `is_dir`: on disk in a working tree, never
    /// in a bare repository.
    fn is_dir(&self, rel: &[u8]) -> bool {
        self.workdir.as_ref().is_some_and(|workdir| {
            os_path::to_path(rel).is_some_and(|rel| workdir.join(rel).is_dir())
        })
    }
}

/// The directory a `.gitattributes` file at `path` (relative to the
/// repository) belongs to: `""` for `.gitattributes`, `a/b` for
/// `a/b/.gitattributes`; `None` for any other path.
pub fn commit_attr_file(path: &[u8]) -> Option<&[u8]> {
    if path == b".gitattributes" {
        return Some(b"");
    }
    path.strip_suffix(b"/.gitattributes")
}

/// The `.gitattributes` files of the index (stage 0), by directory.
fn index_attr_files(repo: &gix::Repository) -> Result<AttributeFiles> {
    let mut files = HashMap::new();
    let Some(index) = repo
        .try_index()
        .map_err(|e| err!("opening the Git index: {}", e))?
    else {
        return Ok(files);
    };
    for entry in index.entries() {
        if entry.stage() != gix::index::entry::Stage::Unconflicted
            || entry.mode == gix::index::entry::Mode::COMMIT
        {
            continue;
        }
        let path = super::git_workdir::tracked_path(entry.path(&index))?;
        if let Some(dir) = commit_attr_file(path.rel()) {
            let data = repo
                .find_object(entry.id)
                .map_err(|e| err!("reading '{}' from the Git index: {}", path.rel(), e))?
                .detach()
                .data;
            files.insert(dir.to_owned(), data);
        }
    }
    Ok(files)
}

/// An attributes file on disk (`GIT_ATTR_FILE_SOURCE_FILE`): libgit2
/// counts one it cannot stat, open or read, a directory, and one larger
/// than `GIT_ATTR_MAX_FILE_SIZE` as missing.
fn read_attr_file(path: &Path) -> Option<Vec<u8>> {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_dir() && meta.len() <= MAX_FILE_SIZE as u64 => {
            std::fs::read(path).ok()
        }
        _ => None,
    }
}

/// `$GIT_DIR/info/attributes` (of the common directory, for a linked
/// worktree).
fn info_attributes(repo: &gix::Repository) -> Option<Vec<u8>> {
    read_attr_file(&repo.common_dir().join("info").join("attributes"))
}

/// libgit2's `cfg_attr_file`: `core.attributesFile`, else
/// `git/attributes` in the XDG configuration directory.
fn global_attributes(repo: &gix::Repository) -> Result<Option<(PathBuf, Vec<u8>)>> {
    let config = repo.config_snapshot();
    let path = match config.trusted_path("core.attributesFile") {
        Some(Ok(path)) => path.into_owned(),
        Some(Err(e)) => return Err(err!("reading 'core.attributesFile': {}", e)),
        None => {
            // `git_sysdir_find_xdg_file("attributes")`.
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|h| !h.is_empty())
                        .map(|h| PathBuf::from(h).join(".config"))
                });
            match xdg {
                Some(dir) => dir.join("git").join("attributes"),
                None => return Ok(None),
            }
        }
    };
    Ok(read_attr_file(&path).map(|bytes| (path, bytes)))
}

fn case(repo: &gix::Repository) -> Case {
    if repo
        .config_snapshot()
        .boolean("core.ignoreCase")
        .unwrap_or(false)
    {
        Case::Fold
    } else {
        Case::Sensitive
    }
}

/// The attributes of every path of one tree (`git_attr_get_ext` asked
/// path by path): the attribute files are parsed once per directory, and
/// every `export-ignore` answer is remembered.
pub struct GitAttributes {
    sources: AttrSources,
    state: Mutex<State>,
}

struct State {
    collection: MetadataCollection,
    /// `$GIT_DIR/info/attributes`, which outranks the rest.
    info: Search,
    /// The user's attributes file, which the rest outrank.
    global: Search,
    /// Per directory: the global file and the `.gitattributes` files of
    /// the directory and those above it, lowest precedence first (gix
    /// searches the lists added last first).
    dirs: HashMap<Vec<u8>, Search>,
    /// Whether each path asked about is export-ignored.
    known: HashMap<Vec<u8>, bool>,
}

/// Where a list of patterns read from `dir`'s `.gitattributes` is made
/// to apply: gix takes a list's base from its file's path relative to a
/// root, here `/`.
fn list_source(dir: &[u8]) -> Result<PathBuf> {
    if dir.is_empty() {
        Ok(PathBuf::from("/.gitattributes"))
    } else {
        os_path::to_path(&[b"/", dir, b"/.gitattributes"].concat())
            .map(Path::to_path_buf)
            .ok_or_else(|| err!("Git attribute path cannot be represented on this platform"))
    }
}

fn parent_dir(dir: &[u8]) -> &[u8] {
    match dir.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => &dir[..slash],
        None => b"",
    }
}

/// What an attribute is for a path (`git_attr_value_t`), when a rule
/// gives it a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrValue {
    /// `name` (`GIT_ATTR_VALUE_TRUE`).
    Set,
    /// `-name` (`GIT_ATTR_VALUE_FALSE`).
    Unset,
    /// `name=value` (`GIT_ATTR_VALUE_STRING`).
    Value(Vec<u8>),
}

impl GitAttributes {
    pub fn new(sources: AttrSources) -> GitAttributes {
        let mut collection = MetadataCollection::default();
        let mut info = Search::default();
        if let Some(bytes) = &sources.info {
            info.add_patterns_buffer(
                bytes,
                PathBuf::from("info/attributes"),
                None,
                &mut collection,
                true,
            );
        }
        let mut global = Search::default();
        if let Some((path, bytes)) = &sources.global {
            global.add_patterns_buffer(bytes, path.clone(), None, &mut collection, true);
        }
        GitAttributes {
            sources,
            state: Mutex::new(State {
                collection,
                info,
                global,
                dirs: HashMap::new(),
                known: HashMap::new(),
            }),
        }
    }

    /// Makes sure the search for the paths directly in `dir` exists,
    /// building those of the directories above it first.
    fn ensure_dir(&self, state: &mut State, dir: &[u8]) -> Result<()> {
        let mut missing = Vec::new();
        let mut cur = dir;
        while !state.dirs.contains_key(cur) {
            missing.push(cur.to_owned());
            if cur.is_empty() {
                break;
            }
            cur = parent_dir(cur);
        }
        for dir in missing.into_iter().rev() {
            let mut search = if dir.is_empty() {
                state.global.clone()
            } else {
                state.dirs[parent_dir(&dir)].clone()
            };
            let workdir_file = self.sources.workdir_file_in(&dir);
            let files = self
                .sources
                .files_in(&dir)
                .into_iter()
                .chain(workdir_file.as_deref());
            for bytes in files {
                // Macros are only defined at the top (`allow_macros`).
                search.add_patterns_buffer(
                    bytes,
                    list_source(&dir)?,
                    Some(Path::new("/")),
                    &mut state.collection,
                    dir.is_empty(),
                );
            }
            state.dirs.insert(dir, search);
        }
        Ok(())
    }

    /// `git_attr_get_ext`: attribute `name` of `path`. `None` when no
    /// rule mentions it, or the rule that decides makes it unspecified
    /// (`!name`).
    pub fn get(&self, path: &CanonPath, name: &str) -> Result<Option<AttrValue>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| err!("an earlier Git attribute lookup panicked"))?;
        self.lookup(&mut state, path, name)
    }

    /// [`GitAttributes::get`] with the state locked.
    fn lookup(&self, state: &mut State, path: &CanonPath, name: &str) -> Result<Option<AttrValue>> {
        let rel = path.rel();
        let dir = parent_dir(rel);
        self.ensure_dir(state, dir)?;
        let is_dir = self.sources.is_dir(rel);
        let case = self.sources.case;
        let mut out = Outcome::default();
        out.initialize_with_selection(&state.collection, Some(name));
        let rel = rel.as_bytes().as_bstr();
        state
            .info
            .pattern_matching_relative_path(rel, case, Some(is_dir), &mut out);
        if !out.is_done() {
            state.dirs[dir].pattern_matching_relative_path(rel, case, Some(is_dir), &mut out);
        }
        let value = out
            .iter_selected()
            .next()
            .and_then(|m| match m.assignment.state {
                StateRef::Set => Some(AttrValue::Set),
                StateRef::Unset => Some(AttrValue::Unset),
                StateRef::Value(value) => Some(AttrValue::Value(value.as_bstr().to_vec())),
                StateRef::Unspecified => None,
            });
        Ok(value)
    }

    /// `isExportIgnored`: whether `export-ignore` is set (without a
    /// value) for `path`.
    pub fn is_ignored(&self, path: &CanonPath) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| err!("an earlier export-ignore lookup panicked"))?;
        if let Some(&known) = state.known.get(path.as_bytes()) {
            return Ok(known);
        }
        let ignored = self.lookup(&mut state, path, "export-ignore")? == Some(AttrValue::Set);
        state.known.insert(path.as_bytes().to_vec(), ignored);
        Ok(ignored)
    }
}

/// A tree with the paths `export-ignore` names hidden
/// (`GitExportIgnoreSourceAccessor`, a `CachingFilteringSourceAccessor`):
/// reading one is an error, and a listing leaves it out.
///
/// `mounts` are the gitlinks of a working tree: Nix mounts an empty
/// directory at each, above the filter, so they and what is below them
/// are not filtered, though their parent's listing is.
pub struct ExportIgnoreAccessor {
    inner: Arc<dyn SourceAccessor>,
    attrs: Arc<GitAttributes>,
    mounts: Vec<CanonPath>,
}

impl ExportIgnoreAccessor {
    pub fn new(
        inner: Arc<dyn SourceAccessor>,
        attrs: Arc<GitAttributes>,
        mounts: Vec<CanonPath>,
    ) -> ExportIgnoreAccessor {
        ExportIgnoreAccessor {
            inner,
            attrs,
            mounts,
        }
    }

    fn is_allowed(&self, path: &CanonPath) -> std::result::Result<bool, AccessorError> {
        if self.mounts.iter().any(|mount| path.is_within(mount)) {
            return Ok(true);
        }
        self.attrs
            .is_ignored(path)
            .map(|ignored| !ignored)
            .map_err(|e| AccessorError::Io(e.to_string().into()))
    }

    /// `checkAccess`, with the filter's `makeNotAllowedError`.
    fn check(&self, path: &CanonPath) -> std::result::Result<(), AccessorError> {
        if self.is_allowed(path)? {
            Ok(())
        } else {
            Err(AccessorError::Restricted(
                [
                    b"'".as_slice(),
                    path.as_bytes(),
                    b"' does not exist because it was fetched with exportIgnore enabled",
                ]
                .concat()
                .into(),
            ))
        }
    }
}

impl SourceAccessor for ExportIgnoreAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        if !self.is_allowed(path)? {
            return Ok(None);
        }
        self.inner.maybe_lstat(path)
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        self.check(path)?;
        self.inner.lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.check(path)?;
        let mut entries = DirEntries::new();
        for (name, kind) in self.inner.read_dir(path)? {
            let child = path
                .join_component(&name)
                .map_err(|e| AccessorError::Io(e.to_string().into()))?;
            // The listing is filtered for the mount points too.
            let hidden = self
                .attrs
                .is_ignored(&child)
                .map_err(|e| AccessorError::Io(e.to_string().into()))?;
            if !hidden {
                entries.insert(name, kind);
            }
        }
        Ok(entries)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        self.check(path)?;
        self.inner.read_file(path)
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> std::result::Result<u64, AccessorError> {
        self.check(path)?;
        self.inner.read_file_to(path, out)
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        self.check(path)?;
        self.inner.read_link(path)
    }

    fn display_prefix(&self) -> &BStr {
        self.inner.display_prefix()
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        self.inner.show_path(path)
    }

    // No `physical_path` and no `precomputed_nar_hash`: both would let
    // the hidden paths be read around the filter.
    fn precomputed_nar_hash(&self, _path: &CanonPath) -> Option<(Hash, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_store::MemoryAccessor;

    fn p(s: &str) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    /// Sources without a repository: a bare one's commit files, or a
    /// working tree at `workdir`.
    fn sources(
        commit: &[(&str, &str)],
        index: &[(&str, &str)],
        workdir: Option<&Path>,
        info: Option<&str>,
    ) -> AttrSources {
        let map = |files: &[(&str, &str)]| {
            files
                .iter()
                .map(|(dir, text)| (dir.as_bytes().to_vec(), text.as_bytes().to_vec()))
                .collect()
        };
        AttrSources {
            workdir: workdir.map(Path::to_path_buf),
            read_workdir: workdir.is_some(),
            index: map(index),
            commit: map(commit),
            info: info.map(|i| i.as_bytes().to_vec()),
            global: None,
            case: Case::Sensitive,
        }
    }

    fn ignored(attrs: &GitAttributes, path: &str) -> bool {
        attrs.is_ignored(&p(path)).unwrap()
    }

    #[test]
    fn export_ignore_follows_gits_precedence() {
        let attrs = GitAttributes::new(sources(
            &[
                (
                    "",
                    "ignored.txt export-ignore\nlogs/*.log export-ignore\nvalued.txt export-ignore=yes\nunset.txt -export-ignore\n*.md export-ignore\n",
                ),
                ("nested", "deep.txt export-ignore\nREADME.md -export-ignore\n"),
            ],
            &[],
            None,
            Some("unset.txt export-ignore\n"),
        ));
        assert!(ignored(&attrs, "/ignored.txt"));
        assert!(
            ignored(&attrs, "/sub/ignored.txt"),
            "a name without / matches at any depth"
        );
        assert!(ignored(&attrs, "/logs/x.log"));
        assert!(!ignored(&attrs, "/logs/keep.txt"));
        assert!(!ignored(&attrs, "/valued.txt"), "a value is not set");
        assert!(ignored(&attrs, "/unset.txt"), "info/attributes comes first");
        assert!(ignored(&attrs, "/nested/deep.txt"));
        assert!(
            !ignored(&attrs, "/deep.txt"),
            "nested rules stay below their directory"
        );
        assert!(ignored(&attrs, "/README.md"));
        assert!(
            !ignored(&attrs, "/nested/README.md"),
            "the nearer file wins"
        );
        assert!(!ignored(&attrs, "/keep.txt"));
    }

    /// The `filter` attribute as `lfs::Fetch::shouldFetch` asks for it:
    /// set, unset, a value, unspecified (`!filter`), and macros defined
    /// at the top only.
    #[test]
    fn attribute_values() {
        let attrs = GitAttributes::new(sources(
            &[
                (
                    "",
                    "[attr]big filter=lfs\n*.bin filter=lfs\n*.dat big\nplain filter\nno -filter\n",
                ),
                (
                    "s",
                    "[attr]small filter=lfs\n*.s small\n*.u big\nx.bin !filter\n",
                ),
            ],
            &[],
            None,
            None,
        ));
        let filter = |path: &str| attrs.get(&p(path), "filter").unwrap();
        let lfs = Some(AttrValue::Value(b"lfs".to_vec()));
        assert_eq!(filter("/a.bin"), lfs);
        assert_eq!(filter("/d.dat"), lfs);
        assert_eq!(
            filter("/s/y.u"),
            lfs,
            "a subdirectory uses the root's macro"
        );
        assert_eq!(filter("/s/x.s"), None, "a subdirectory defines no macro");
        assert_eq!(filter("/s/x.bin"), None);
        assert_eq!(filter("/plain"), Some(AttrValue::Set));
        assert_eq!(filter("/no"), Some(AttrValue::Unset));
        assert_eq!(filter("/other"), None);
    }

    /// A pattern for directories only (`dir/`) needs the directory on
    /// disk: in a bare repository it never matches.
    #[test]
    fn directory_patterns_need_the_working_tree() {
        let commit = [("", "dironly/ export-ignore\n")];
        let bare = GitAttributes::new(sources(&commit, &[], None, None));
        assert!(!ignored(&bare, "/dironly"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dironly")).unwrap();
        let work = GitAttributes::new(sources(&commit, &[], Some(dir.path()), None));
        assert!(ignored(&work, "/dironly"));
        // What is below it is not: attributes of a directory do not
        // apply to its contents.
        assert!(!ignored(&work, "/dironly/a"));
    }

    /// In a checkout, the working tree's `.gitattributes` outranks the
    /// index's and the commit's at the same level, as libgit2 reads them.
    #[test]
    fn a_checkouts_own_attributes_apply_to_its_commits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "keep.txt export-ignore\n",
        )
        .unwrap();
        let attrs = GitAttributes::new(sources(
            &[("", "keep.txt -export-ignore\nother export-ignore\n")],
            &[("", "other -export-ignore\n")],
            Some(dir.path()),
            None,
        ));
        assert!(ignored(&attrs, "/keep.txt"));
        assert!(!ignored(&attrs, "/other"), "the index outranks the commit");
    }

    /// A working-tree `.gitattributes` that cannot be read (here a
    /// directory) counts as missing, as libgit2 has it, and so does an
    /// attributes file larger than `GIT_ATTR_MAX_FILE_SIZE`.
    #[test]
    fn unreadable_and_huge_attribute_files_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".gitattributes")).unwrap();
        let attrs = GitAttributes::new(sources(
            &[("", "a export-ignore\n")],
            &[],
            Some(dir.path()),
            None,
        ));
        assert!(ignored(&attrs, "/a"));
        assert_eq!(read_attr_file(&dir.path().join(".gitattributes")), None);
        assert_eq!(read_attr_file(&dir.path().join("missing")), None);

        let mut huge = "a export-ignore\n".repeat(MAX_FILE_SIZE / 16);
        huge.push_str("padding\n");
        assert!(huge.len() > MAX_FILE_SIZE);
        let attrs = GitAttributes::new(sources(&[("", &huge)], &[], None, None));
        assert!(!ignored(&attrs, "/a"));
    }

    #[test]
    fn hidden_paths_are_missing_from_listings_and_unreadable() {
        let mut tree = MemoryAccessor::new();
        tree.add_file(&p("/keep"), b"k".to_vec(), false).unwrap();
        tree.add_file(&p("/gone"), b"g".to_vec(), false).unwrap();
        tree.add_file(&p("/dir/inner"), b"i".to_vec(), false)
            .unwrap();
        let attrs = GitAttributes::new(sources(
            &[("", "gone export-ignore\ndir export-ignore\n")],
            &[],
            None,
            None,
        ));
        let acc = ExportIgnoreAccessor::new(Arc::new(tree), Arc::new(attrs), Vec::new());
        let root: Vec<_> = acc
            .read_dir(&CanonPath::root())
            .unwrap()
            .into_keys()
            .collect();
        assert_eq!(root, ["keep"]);
        assert_eq!(
            acc.read_file(&p("/gone")).unwrap_err().to_string(),
            "'/gone' does not exist because it was fetched with exportIgnore enabled"
        );
        assert!(acc.maybe_lstat(&p("/gone")).unwrap().is_none());
        assert!(!acc.path_exists(&p("/dir")).unwrap());
        // Below a hidden directory, a path that is not hidden itself can
        // still be read, as in Nix.
        assert_eq!(acc.read_file(&p("/dir/inner")).unwrap(), b"i");
    }
}
