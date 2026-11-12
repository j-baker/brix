//! The evaluator's root file system: the host's root with fetched trees
//! mounted under the store directory (`storeFS->mount` in Nix).

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use brix_store::{AccessorError, CanonPath, DirEntries, FileType, SourceAccessor, Stat};
use bstr::{BStr, BString};
use hashbrown::HashMap;

use crate::error::msg;

/// Wraps the host's root accessor and lets the evaluator mount trees
/// (fetched inputs, built outputs, copied sources) at store paths. The
/// nearest mount point at or above a path serves it; everything else
/// falls through to the host's root.
///
/// Metadata lookups are memoised. Everything reachable during pure
/// evaluation is immutable for the evaluation's lifetime (store paths and
/// fetched trees are content-addressed; the import cache already assumes
/// as much), and Nix resolves symlinks component by component on every
/// `import`, `readFile`, `pathExists` and `readDir`: evaluating
/// `hello.drvPath` issued ten thousand `lstat`s for 141 imported files.
/// With the cache each distinct path is stat'ed once.
pub struct RootAccessor {
    root: Arc<dyn SourceAccessor>,
    mounts: RwLock<Mounts>,
    cache: Mutex<Cache>,
}

/// The trees mounted on the root.
#[derive(Default)]
struct Mounts {
    /// Each tree by its mount point. Every path copied to the store is
    /// mounted at its store path, hundreds in a nixpkgs evaluation, so a
    /// path's mount is found by looking up its ancestors, never by
    /// scanning the table.
    at: HashMap<CanonPath, Arc<dyn SourceAccessor>>,
    /// The names of the mount points directly below each directory, which
    /// its listing shows as directories.
    children: HashMap<CanonPath, BTreeSet<BString>>,
}

/// What [`RootAccessor`] remembers about paths.
#[derive(Default)]
struct Cache {
    lstat: HashMap<CanonPath, Option<Stat>>,
    link: HashMap<CanonPath, BString>,
    dir: HashMap<CanonPath, Arc<DirEntries>>,
    /// Every path with an entry above, and all of its ancestors: a mount
    /// point outside this set has nothing remembered at or below it.
    covered: hashbrown::HashSet<BString>,
}

impl Cache {
    /// Records that `path` has an entry.
    fn cover(&mut self, path: &CanonPath) {
        for p in path.ancestors() {
            // The ancestors of a covered path are covered.
            if self.covered.contains(p) {
                break;
            }
            self.covered.insert(p.to_owned());
        }
    }

    /// Forgets what mounting a tree at `at` changes: whatever is at or
    /// below `at`, and the listing of its parent. A store path is mounted
    /// as it is created, before anything can have looked at it, so
    /// usually only the parent's listing goes.
    fn forget_for_mount(&mut self, at: &CanonPath) {
        if self.covered.contains(at.abs()) {
            *self = Cache::default();
        } else if let Some(parent) = at.ancestors().nth(1) {
            self.dir.remove(parent);
        }
    }
}

impl RootAccessor {
    pub fn new(root: Arc<dyn SourceAccessor>) -> RootAccessor {
        RootAccessor {
            root,
            mounts: RwLock::new(Mounts::default()),
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn mount(&self, at: CanonPath, accessor: Arc<dyn SourceAccessor>) {
        self.cache
            .lock()
            .expect("cache poisoned")
            .forget_for_mount(&at);
        let mut mounts = self.mounts.write().expect("mount table poisoned");
        if let (Some(parent), Some(name)) = (at.parent(), at.base_name()) {
            mounts
                .children
                .entry(parent)
                .or_default()
                .insert(name.to_owned());
        }
        mounts.at.insert(at, accessor);
    }

    #[cfg(test)]
    pub fn is_mounted(&self, at: &CanonPath) -> bool {
        self.mounts
            .read()
            .expect("mount table poisoned")
            .at
            .contains_key(at)
    }

    /// Runs `f` on the mount that serves `path`, refusing to recurse
    /// indefinitely. A mounted tree may itself read through this root
    /// (a copied source is served from its origin), so a mount that
    /// resolves back into its own mount point would otherwise recurse
    /// until the stack overflows; such a cycle is a brix bug, reported as
    /// an error naming the path rather than an abort.
    fn delegate<T>(
        &self,
        path: &CanonPath,
        f: impl FnOnce(&dyn SourceAccessor, &CanonPath) -> Result<T, AccessorError>,
    ) -> Result<T, AccessorError> {
        thread_local! {
            static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        const MAX_DEPTH: usize = 64;
        let depth = DEPTH.with(|d| d.get());
        if depth >= MAX_DEPTH {
            return Err(AccessorError::Io(
                msg!(
                    "mount cycle while resolving '",
                    path.abs(),
                    "' (a mounted tree is served from inside itself)"
                )
                .into(),
            ));
        }
        DEPTH.with(|d| d.set(depth + 1));
        let (acc, sub) = self.resolve(path);
        let result = f(&*acc, &sub);
        DEPTH.with(|d| d.set(depth));
        result
    }

    /// The accessor serving `path` and the path within it: the tree
    /// mounted at the nearest of `path` and its ancestors that is a mount
    /// point (as Nix's `MountedSourceAccessor::resolve` finds it), else
    /// the host's root.
    fn resolve(&self, path: &CanonPath) -> (Arc<dyn SourceAccessor>, CanonPath) {
        let mounts = self.mounts.read().expect("mount table poisoned");
        match path
            .ancestors()
            .find_map(|point| mounts.at.get_key_value(point))
        {
            Some((at, acc)) => (acc.clone(), path.remove_prefix(at)),
            None => (self.root.clone(), path.clone()),
        }
    }
}

impl SourceAccessor for RootAccessor {
    /// Answered from the `maybe_lstat` cache; only a missing path goes to
    /// the mounted accessor, which may know why it is missing.
    fn lstat(&self, path: &CanonPath) -> Result<Stat, AccessorError> {
        match self.maybe_lstat(path)? {
            Some(st) => Ok(st),
            None => self.delegate(path, |acc, sub| acc.lstat(sub)),
        }
    }

    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        if let Some(st) = self.cache.lock().expect("cache poisoned").lstat.get(path) {
            return Ok(*st);
        }
        let st = self.delegate(path, |acc, sub| acc.maybe_lstat(sub))?;
        let mut cache = self.cache.lock().expect("cache poisoned");
        cache.lstat.insert(path.clone(), st);
        cache.cover(path);
        Ok(st)
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        if let Some(entries) = self.cache.lock().expect("cache poisoned").dir.get(path) {
            return Ok((**entries).clone());
        }
        let mut entries = self.delegate(path, |acc, sub| acc.read_dir(sub))?;
        // Mount points directly below `path` appear as directories.
        if let Some(names) = self
            .mounts
            .read()
            .expect("mount table poisoned")
            .children
            .get(path)
        {
            for name in names {
                entries
                    .entry(name.clone())
                    .or_insert(Some(FileType::Directory));
            }
        }
        let mut cache = self.cache.lock().expect("cache poisoned");
        cache.dir.insert(path.clone(), Arc::new(entries.clone()));
        cache.cover(path);
        Ok(entries)
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        self.delegate(path, |acc, sub| acc.read_file(sub))
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, AccessorError> {
        self.delegate(path, |acc, sub| acc.read_file_to(sub, out))
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        if let Some(target) = self.cache.lock().expect("cache poisoned").link.get(path) {
            return Ok(target.clone());
        }
        let target = self.delegate(path, |acc, sub| acc.read_link(sub))?;
        let mut cache = self.cache.lock().expect("cache poisoned");
        cache.link.insert(path.clone(), target.clone());
        cache.cover(path);
        Ok(target)
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new("")
    }

    /// Mounted trees decide how their paths are shown (a fetched input
    /// shows as `«github:…»/file`, a lazily mounted directory as its real
    /// location); everything else is shown as is.
    fn show_path(&self, path: &CanonPath) -> BString {
        let (acc, sub) = self.resolve(path);
        if std::sync::Arc::ptr_eq(&acc, &self.root) {
            path.abs().to_owned()
        } else {
            acc.show_path(&sub)
        }
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        let (acc, sub) = self.resolve(path);
        acc.physical_path(&sub)
    }

    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(brix_store::Hash, u64)> {
        let (acc, sub) = self.resolve(path);
        acc.precomputed_nar_hash(&sub)
    }

    fn remembered_nar_hash(
        &self,
        path: &CanonPath,
    ) -> Result<Option<brix_store::Hash>, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.remembered_nar_hash(&sub)
    }
}

/// The accessor holding `<nix/fetchurl.nix>`, shown as `<nix/…>`
/// (`corepkgsFS->setPathDisplay("<nix", ">")`).
pub struct CorepkgsAccessor {
    inner: brix_store::MemoryAccessor,
}

impl CorepkgsAccessor {
    pub fn new(inner: brix_store::MemoryAccessor) -> CorepkgsAccessor {
        CorepkgsAccessor { inner }
    }
}

impl SourceAccessor for CorepkgsAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        self.inner.maybe_lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        self.inner.read_file(path)
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        self.inner.read_link(path)
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new("<nix")
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        msg!("<nix", path.abs(), ">").into()
    }
}

/// An accessor whose paths are shown with a prefix, as Nix shows files
/// inside a fetched input: `«github:owner/repo/rev»/path`.
pub struct DisplayPrefixAccessor {
    inner: Arc<dyn SourceAccessor>,
    prefix: BString,
}

impl DisplayPrefixAccessor {
    /// `inner` with its paths shown after `prefix` (bytes: a directory's
    /// name need not be UTF-8).
    pub fn new(
        inner: Arc<dyn SourceAccessor>,
        prefix: impl Into<BString>,
    ) -> DisplayPrefixAccessor {
        DisplayPrefixAccessor {
            inner,
            prefix: prefix.into(),
        }
    }

    /// An error of the inner accessor about `path`, naming the path as
    /// this accessor shows it. The inner accessor names it as it shows it
    /// (a fetched input's cache directory), where Nix names it under the
    /// input's prefix. A reason given in full (`Restricted`), a loop and
    /// an I/O error are kept as they are.
    fn shown(&self, path: &CanonPath, e: AccessorError) -> AccessorError {
        use AccessorError::*;
        match e {
            NotFound(_) => NotFound(self.show_path(path)),
            NotADirectory(_) => NotADirectory(self.show_path(path)),
            NotARegularFile(_) => NotARegularFile(self.show_path(path)),
            NotASymlink(_) => NotASymlink(self.show_path(path)),
            IsASymlink(_) => IsASymlink(self.show_path(path)),
            PathIsASymlink(_) => PathIsASymlink(self.show_path(path)),
            NonUtf8(_) => NonUtf8(self.show_path(path)),
            other => other,
        }
    }
}

impl SourceAccessor for DisplayPrefixAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        self.inner
            .maybe_lstat(path)
            .map_err(|e| self.shown(path, e))
    }

    fn lstat(&self, path: &CanonPath) -> Result<Stat, AccessorError> {
        self.inner.lstat(path).map_err(|e| self.shown(path, e))
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        self.inner.read_dir(path).map_err(|e| self.shown(path, e))
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        self.inner.read_file(path).map_err(|e| self.shown(path, e))
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, AccessorError> {
        self.inner
            .read_file_to(path, out)
            .map_err(|e| self.shown(path, e))
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        self.inner.read_link(path).map_err(|e| self.shown(path, e))
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new(&self.prefix)
    }

    /// The prefix and the path, unless the path lies in a tree mounted in
    /// the inner one that shows its own paths (a Git submodule's, shown
    /// as the submodule's input: `MountedSourceAccessor::showPath`).
    fn show_path(&self, path: &CanonPath) -> BString {
        match self.inner.mounted_show_path(path) {
            Some(shown) => shown,
            None => {
                let abs = path.abs_or_empty();
                let mut shown = Vec::with_capacity(self.prefix.len() + abs.len());
                shown.extend_from_slice(&self.prefix);
                shown.extend_from_slice(abs);
                shown.into()
            }
        }
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        self.inner.physical_path(path)
    }

    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(brix_store::Hash, u64)> {
        self.inner.precomputed_nar_hash(path)
    }

    fn remembered_nar_hash(
        &self,
        path: &CanonPath,
    ) -> Result<Option<brix_store::Hash>, AccessorError> {
        self.inner.remembered_nar_hash(path)
    }
}

/// A subtree of another accessor shown as a root: what Nix's
/// `copyPathToStore` makes readable at the resulting store path. Paths
/// are resolved against `root` on `inner`; nothing is copied.
pub struct SubtreeAccessor {
    inner: Arc<dyn SourceAccessor>,
    root: CanonPath,
}

impl SubtreeAccessor {
    pub fn new(inner: Arc<dyn SourceAccessor>, root: CanonPath) -> SubtreeAccessor {
        SubtreeAccessor { inner, root }
    }

    fn below(&self, path: &CanonPath) -> CanonPath {
        self.root.join(path)
    }
}

impl SourceAccessor for SubtreeAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        self.inner.maybe_lstat(&self.below(path))
    }

    fn lstat(&self, path: &CanonPath) -> Result<Stat, AccessorError> {
        self.inner.lstat(&self.below(path))
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        self.inner.read_dir(&self.below(path))
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        self.inner.read_file(&self.below(path))
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        self.inner.read_link(&self.below(path))
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        self.inner.physical_path(&self.below(path))
    }
}

/// A filtered copy (`builtins.path { filter = …; }`, `filterSource`) as
/// Nix would have written it to the store: only the entries the filter
/// accepted exist. `included` holds the accepted paths on `inner` (the
/// NAR dump consults the filter for every entry it writes, so an entry is
/// present exactly when it and all its ancestors were accepted); the root
/// itself is always present.
pub struct FilteredAccessor {
    inner: SubtreeAccessor,
    included: HashSet<CanonPath>,
}

impl FilteredAccessor {
    pub fn new(inner: SubtreeAccessor, included: HashSet<CanonPath>) -> FilteredAccessor {
        FilteredAccessor { inner, included }
    }

    fn check(&self, path: &CanonPath) -> Result<(), AccessorError> {
        if path.is_root() || self.included.contains(&self.inner.below(path)) {
            Ok(())
        } else {
            Err(AccessorError::NotFound(self.inner.show_path(path)))
        }
    }
}

impl SourceAccessor for FilteredAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        if self.check(path).is_err() {
            return Ok(None);
        }
        self.inner.maybe_lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        self.check(path)?;
        let entries = self.inner.read_dir(path)?;
        Ok(entries
            .into_iter()
            .filter(|(name, _)| {
                self.inner
                    .below(path)
                    .join_component(name)
                    .is_ok_and(|p| self.included.contains(&p))
            })
            .collect())
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        self.check(path)?;
        self.inner.read_file(path)
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        self.check(path)?;
        self.inner.read_link(path)
    }

    fn physical_path(&self, _path: &CanonPath) -> Option<std::path::PathBuf> {
        None
    }
}

/// A store path's text (`<store dir>/<base name>`) as a path on the root
/// accessor, where the store object is mounted and allowed.
pub(crate) fn store_path_canon(text: &str) -> CanonPath {
    CanonPath::new(text)
        .expect("a store path's text holds no NUL byte, the only thing CanonPath::new rejects")
}

impl<'a> crate::evaluator::Evaluator<'a> {
    /// [`store_path_canon`] for `sp` in this evaluator's store directory.
    pub(crate) fn store_path_mount_point(&self, sp: &brix_store::StorePath) -> CanonPath {
        store_path_canon(&self.config.store_dir.display(sp))
    }

    /// Makes one file readable at its store path for the rest of the
    /// evaluation, as it would be once written (`toFile`, `fetchurl`).
    pub(crate) fn mount_store_file(&self, sp: &brix_store::StorePath, contents: Vec<u8>) {
        let mut file = brix_store::MemoryAccessor::new();
        file.add_file(&CanonPath::root(), contents, false)
            .expect("an empty tree takes a file at its root");
        self.mount(self.store_path_mount_point(sp), Arc::new(file));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use brix_store::MemoryAccessor;

    fn p(s: &str) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn tree(files: &[(&str, &str)]) -> Arc<dyn SourceAccessor> {
        let mut mem = MemoryAccessor::new();
        for (path, contents) in files {
            mem.add_file(&p(path), contents.as_bytes(), false).unwrap();
        }
        Arc::new(mem)
    }

    fn read(root: &RootAccessor, path: &str) -> String {
        String::from_utf8(root.read_file(&p(path)).unwrap()).unwrap()
    }

    fn file_type(root: &RootAccessor, path: &str) -> Option<FileType> {
        root.maybe_lstat(&p(path)).unwrap().map(|st| st.file_type)
    }

    #[test]
    fn the_nearest_mount_point_serves_a_path() {
        let root = RootAccessor::new(tree(&[("/store/a-src/f", "host"), ("/f", "top")]));
        root.mount(p("/store/a"), tree(&[("/f", "a")]));
        root.mount(p("/store/a/inner"), tree(&[("/f", "inner")]));
        assert_eq!(read(&root, "/store/a/f"), "a");
        assert_eq!(read(&root, "/store/a/inner/f"), "inner");
        assert_eq!(file_type(&root, "/store/a"), Some(FileType::Directory));
        // `/store/a` is a prefix of the name `a-src`, not one of its
        // ancestors: the host's root serves it.
        assert_eq!(read(&root, "/store/a-src/f"), "host");
        assert_eq!(read(&root, "/f"), "top");
        assert_eq!(file_type(&root, "/store/a/missing"), None);
    }

    #[test]
    fn a_mount_at_the_root_serves_everything_not_mounted_deeper() {
        let root = RootAccessor::new(tree(&[("/f", "host")]));
        root.mount(CanonPath::root(), tree(&[("/f", "mounted")]));
        root.mount(p("/d"), tree(&[("/f", "d")]));
        assert_eq!(read(&root, "/f"), "mounted");
        assert_eq!(read(&root, "/d/f"), "d");
    }

    #[test]
    fn mounting_forgets_what_was_cached_below_the_mount_point() {
        let root = RootAccessor::new(tree(&[]));
        assert_eq!(file_type(&root, "/store/a/f"), None);
        root.mount(p("/store/a"), tree(&[("/f", "1")]));
        assert!(root.is_mounted(&p("/store/a")));
        assert_eq!(file_type(&root, "/store/a/f"), Some(FileType::Regular));
        // A second mount at the same point replaces the first.
        root.mount(p("/store/a"), tree(&[("/g", "2")]));
        assert_eq!(file_type(&root, "/store/a/f"), None);
        assert_eq!(read(&root, "/store/a/g"), "2");
        assert!(!root.is_mounted(&p("/store")));
    }

    /// A tree that counts the lookups reaching it.
    struct Counting {
        inner: Arc<dyn SourceAccessor>,
        lookups: AtomicUsize,
    }

    impl SourceAccessor for Counting {
        fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            self.inner.maybe_lstat(path)
        }

        fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            self.inner.read_dir(path)
        }

        fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
            self.inner.read_file(path)
        }

        fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            self.inner.read_link(path)
        }
    }

    /// Mounting a new store path changes only its parent's listing: what
    /// is known about other paths still holds and is not looked up again.
    #[test]
    fn mounting_a_new_path_keeps_what_is_known_elsewhere() {
        let host = Arc::new(Counting {
            inner: tree(&[("/store/src/f", "")]),
            lookups: AtomicUsize::new(0),
        });
        let root = RootAccessor::new(host.clone());
        let names =
            |path: &str| -> Vec<BString> { root.read_dir(&p(path)).unwrap().into_keys().collect() };
        assert_eq!(file_type(&root, "/store/src/f"), Some(FileType::Regular));
        assert_eq!(file_type(&root, "/store"), Some(FileType::Directory));
        assert_eq!(names("/store"), ["src"]);
        let looked_up = host.lookups.load(Ordering::Relaxed);

        root.mount(p("/store/new"), tree(&[("/g", "")]));
        assert_eq!(file_type(&root, "/store/src/f"), Some(FileType::Regular));
        assert_eq!(file_type(&root, "/store"), Some(FileType::Directory));
        assert_eq!(host.lookups.load(Ordering::Relaxed), looked_up);
        // The parent's listing is read again, with the mount point in it.
        assert_eq!(names("/store"), ["new", "src"]);
        assert_eq!(host.lookups.load(Ordering::Relaxed), looked_up + 1);
        assert_eq!(file_type(&root, "/store/new/g"), Some(FileType::Regular));
    }

    #[test]
    fn mount_points_are_listed_in_their_parent_directory() {
        let root = RootAccessor::new(tree(&[("/store/x", "")]));
        let names =
            |path: &str| -> Vec<BString> { root.read_dir(&p(path)).unwrap().into_keys().collect() };
        assert_eq!(names("/store"), ["x"]);
        root.mount(p("/store/b"), tree(&[("/f", "")]));
        root.mount(p("/store/b/deeper/still"), tree(&[("/f", "")]));
        assert_eq!(names("/store"), ["b", "x"]);
        assert_eq!(names("/store/b"), ["f"]);
        root.mount(p("/store/b/deeper"), tree(&[("/g", "")]));
        assert_eq!(names("/store/b"), ["deeper", "f"]);
        assert_eq!(names("/store/b/deeper"), ["g", "still"]);
    }

    /// Names are bytes: a mount point whose name is not UTF-8 is listed
    /// in its parent in unsigned byte order (after `z`), is served and
    /// remembered under its own bytes, and a prefix is shown as it is.
    #[test]
    fn names_and_prefixes_are_bytes() {
        let root = RootAccessor::new(tree(&[("/store/z", "")]));
        let latin1 = CanonPath::new(b"/store/\xe9").unwrap();
        root.mount(latin1.clone(), tree(&[("/f", "e9")]));
        let names: Vec<BString> = root.read_dir(&p("/store")).unwrap().into_keys().collect();
        assert_eq!(names, [BString::from("z"), BString::from(&b"\xe9"[..])]);
        let file = latin1.join_component("f").unwrap();
        assert_eq!(root.read_file(&file).unwrap(), b"e9");
        assert_eq!(
            root.maybe_lstat(&file).unwrap().map(|st| st.file_type),
            Some(FileType::Regular)
        );
        let shown = DisplayPrefixAccessor::new(tree(&[("/f", "")]), &b"/d\xe9"[..]);
        let caf = CanonPath::new(b"/caf\xe9").unwrap();
        assert_eq!(shown.show_path(&caf), &b"/d\xe9/caf\xe9"[..]);
        assert_eq!(shown.display_prefix(), &b"/d\xe9"[..]);
    }
}
