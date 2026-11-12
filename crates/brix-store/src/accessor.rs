//! Reading files through an abstraction, not the file system.
//!
//! Mirrors `src/libutil/source-accessor.hh` and its memory, mounted and
//! POSIX implementations. Every file brix reads (Nix sources, `readFile`,
//! `readDir`, paths added to the store, NAR dumps) goes through
//! [`SourceAccessor`], which is why the core crates compile for wasm: the
//! host decides what a path means.
//!
//! Names are bytes, as in Nix: a directory's entries ([`DirEntries`]), a
//! symlink's target ([`SourceAccessor::read_link`]) and what a path is
//! shown as ([`SourceAccessor::show_path`]) are byte strings, whatever
//! their bytes. An error's `Display` is lossy; the message Nix shows is
//! [`AccessorError::message_bytes`].
//!
//! Accessors are identified by a small [`AccessorId`] from an
//! [`AccessorRegistry`] so that a path value can be stored as
//! `(id, path)` without owning an `Arc`. Nix's `SourceAccessor::number`
//! plays the same role.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::sync::Arc;

use bstr::{BStr, BString, ByteSlice};

use crate::canon_path::CanonPath;
use crate::hash::Hash;

/// `SourceAccessor::Type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileType {
    Regular,
    Symlink,
    Directory,
    /// Device nodes, sockets, fifos: things that exist but cannot be put
    /// in a NAR. `builtins.readFileType` reports them as `unknown`.
    Char,
    Block,
    Socket,
    Fifo,
    Unknown,
}

/// `SourceAccessor::Stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub file_type: FileType,
    /// The size of a regular file, when known cheaply.
    pub size: Option<u64>,
    /// Only meaningful for regular files: is the owner-execute bit set?
    pub executable: bool,
}

/// A directory listing: name to type (`None` when the accessor does not
/// know the type without another lookup). Names are bytes, ordered as
/// Nix's `std::map<std::string, …>` orders them: unsigned bytes, the
/// order of NAR entries and of `builtins.readDir`.
pub type DirEntries = BTreeMap<BString, Option<FileType>>;

/// Errors from accessors, with Nix's messages (`FileNotFound`,
/// `NotADirectory`, `NotARegularFile`, `NotASymlink`). Each holds what the
/// accessor shows the path as ([`SourceAccessor::show_path`]), or the
/// whole message, as bytes: `Display` replaces those that are not UTF-8,
/// [`AccessorError::message_bytes`] keeps them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessorError {
    #[error("path '{0}' does not exist")]
    NotFound(BString),
    #[error("file '{0}' is not a directory")]
    NotADirectory(BString),
    #[error("file '{0}' is not a regular file")]
    NotARegularFile(BString),
    #[error("file '{0}' is not a symbolic link")]
    NotASymlink(BString),
    /// `SymlinkNotAllowed` as `MemorySourceAccessor` words it.
    #[error("file '{0}' is a symlink")]
    IsASymlink(BString),
    /// `SymlinkNotAllowed` as `PosixSourceAccessor` words it.
    #[error("path '{0}' is a symlink")]
    PathIsASymlink(BString),
    #[error("infinite symlink recursion in path '{0}'")]
    SymlinkLoop(BString),
    #[error("file '{0}' cannot be created because some parent directories don't exist")]
    MissingParent(BString),
    /// A file name or symlink target the operating system does not give
    /// as bytes: only from [`FsAccessor`] where that is not Unix (on
    /// Unix every name is bytes, as in Nix).
    #[error("file name or symlink target in '{0}' is not valid UTF-8")]
    NonUtf8(BString),
    /// `RestrictedPathError`: the accessor hides this path and says why
    /// (an untracked file in a Git checkout). The message is complete.
    #[error("{0}")]
    Restricted(BString),
    #[error("{0}")]
    Io(BString),
    /// The error with a "while …" trace added (`addTrace`), by an
    /// accessor that does work of its own when a file is read (a Git LFS
    /// file is fetched then: "while smudging git-lfs file '…'"). Displays
    /// as the inner error's message; [`AccessorError::traces`] gives the
    /// traces. Boxed, so that the error is no larger for it.
    #[error("{0}")]
    Traced(Box<TracedAccessorError>),
}

/// An [`AccessorError`] and the trace added to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracedAccessorError {
    pub trace: String,
    pub inner: AccessorError,
}

impl fmt::Display for TracedAccessorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl AccessorError {
    /// `inner` with the trace `trace` added.
    pub fn traced(inner: AccessorError, trace: impl Into<String>) -> AccessorError {
        AccessorError::Traced(Box::new(TracedAccessorError {
            trace: trace.into(),
            inner,
        }))
    }

    /// The error without its traces.
    pub fn untraced(&self) -> &AccessorError {
        let mut cur = self;
        while let AccessorError::Traced(traced) = cur {
            cur = &traced.inner;
        }
        cur
    }

    /// The traces, innermost (added first) first: the order a caller adds
    /// them to its own error in.
    pub fn traces(&self) -> Vec<&str> {
        let mut traces = Vec::new();
        let mut cur = self;
        while let AccessorError::Traced(traced) = cur {
            traces.push(traced.trace.as_str());
            cur = &traced.inner;
        }
        traces.reverse();
        traces
    }

    /// The message as Nix writes it: the `Display` text, with the path's
    /// bytes as they are. A traced error's is its inner error's, as its
    /// `Display` is.
    pub fn message_bytes(&self) -> Vec<u8> {
        let quoted = |before: &str, path: &[u8], after: &str| {
            [before.as_bytes(), path, after.as_bytes()].concat()
        };
        match self {
            AccessorError::NotFound(p) => quoted("path '", p, "' does not exist"),
            AccessorError::NotADirectory(p) => quoted("file '", p, "' is not a directory"),
            AccessorError::NotARegularFile(p) => quoted("file '", p, "' is not a regular file"),
            AccessorError::NotASymlink(p) => quoted("file '", p, "' is not a symbolic link"),
            AccessorError::IsASymlink(p) => quoted("file '", p, "' is a symlink"),
            AccessorError::PathIsASymlink(p) => quoted("path '", p, "' is a symlink"),
            AccessorError::SymlinkLoop(p) => quoted("infinite symlink recursion in path '", p, "'"),
            AccessorError::MissingParent(p) => quoted(
                "file '",
                p,
                "' cannot be created because some parent directories don't exist",
            ),
            AccessorError::NonUtf8(p) => quoted(
                "file name or symlink target in '",
                p,
                "' is not valid UTF-8",
            ),
            AccessorError::Restricted(message) | AccessorError::Io(message) => message.to_vec(),
            AccessorError::Traced(traced) => traced.inner.message_bytes(),
        }
    }
}

/// `SourceAccessor`. Implementations provide the four primitives; the
/// rest is derived.
///
/// Symlinks are never followed: reading a file or a directory at a path
/// whose last component is a symlink is an error
/// ([`AccessorError::IsASymlink`] or [`AccessorError::PathIsASymlink`],
/// Nix's `SymlinkNotAllowed`), and only [`SourceAccessor::read_link`]
/// and [`SourceAccessor::maybe_lstat`] look at a symlink itself. A caller
/// that wants what a path points to resolves it first
/// ([`SourceAccessor::resolve_symlinks`], `SourcePath::resolveSymlinks`
/// in Nix), so the paths it reads have no symlinks among their ancestors
/// either.
pub trait SourceAccessor: Send + Sync {
    /// `maybeLstat`: metadata without following a final symlink, or
    /// `None` if nothing exists at `path`.
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError>;

    /// `readDirectory`. A symlink at `path` is refused.
    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError>;

    /// `readFile`: the whole contents of a regular file. A symlink at
    /// `path` is refused.
    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError>;

    /// `readLink`: a symlink's target, verbatim (bytes, as in Nix).
    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError>;

    /// `readFile(path, sink)`: stream a file; returns the byte count.
    /// The default reads it whole; file-system accessors override this.
    fn read_file_to(&self, path: &CanonPath, out: &mut dyn Write) -> Result<u64, AccessorError> {
        let data = self.read_file(path)?;
        out.write_all(&data)
            .map_err(|e| AccessorError::Io(e.to_string().into()))?;
        Ok(data.len() as u64)
    }

    /// `displayPrefix`: what is printed before a path in messages, e.g.
    /// the store path a fetched tree was mounted as. Nix's default is
    /// `«unknown»`; the root file system accessor uses `""`.
    fn display_prefix(&self) -> &BStr {
        BStr::new("«unknown»")
    }

    /// `showPath`: the display prefix and the path's absolute form, as
    /// bytes (`displayPrefix + path.abs()`, which Nix prints as they are).
    fn show_path(&self, path: &CanonPath) -> BString {
        let prefix = self.display_prefix();
        let mut shown = Vec::with_capacity(prefix.len() + path.as_bytes().len());
        shown.extend_from_slice(prefix);
        shown.extend_from_slice(path.as_bytes());
        BString::from(shown)
    }

    /// `getPhysicalPath`: where this path lives on the real file system,
    /// if anywhere.
    fn physical_path(&self, _path: &CanonPath) -> Option<std::path::PathBuf> {
        None
    }

    /// The SHA-256 NAR hash and NAR size of the tree at `path`, when the
    /// accessor already knows them (a compiled flake-input image stores
    /// them for every directory). `None` means "compute it"; an accessor
    /// must only answer when the answer is exactly what hashing the tree
    /// would give, as the result becomes a store path.
    fn precomputed_nar_hash(&self, _path: &CanonPath) -> Option<(Hash, u64)> {
        None
    }

    /// The SHA-256 NAR hash Nix's `fetchToStore` takes for the tree at
    /// `path` when it copies it: the one remembered for the tree's
    /// [`SourceAccessor::fingerprint`], which need not be the tree's own
    /// (a fingerprint that does not tell all is shared by trees that
    /// differ), or the tree's own, computed now and remembered. `None`
    /// for a tree that remembers nothing; then the caller hashes it.
    /// Wrappers forward it.
    fn remembered_nar_hash(&self, _path: &CanonPath) -> Result<Option<Hash>, AccessorError> {
        Ok(None)
    }

    /// `SourceAccessor::fingerprint` (`getFingerprint` of the root): a
    /// string identifying the whole tree, when the accessor knows one. A
    /// Git commit's export knows how it was made (`git:<rev>;e;legacy`,
    /// `;s` with submodules mounted), which the input it was fetched for
    /// does not say. `None` leaves the fingerprint to the input.
    fn fingerprint(&self) -> Option<&str> {
        None
    }

    /// How `path` is shown when it lies in a tree mounted inside this one
    /// that shows its paths its own way (`MountedSourceAccessor::showPath`
    /// going to the mount: a Git submodule's files are shown as the
    /// submodule's input). Bytes, as [`SourceAccessor::show_path`]'s are.
    /// `None` when `path` is this accessor's own, so that whoever shows
    /// this tree under a prefix of its own keeps doing so. Wrappers
    /// forward it.
    fn mounted_show_path(&self, _path: &CanonPath) -> Option<BString> {
        None
    }

    /// `lstat`: like [`SourceAccessor::maybe_lstat`] but missing paths
    /// are an error. Accessors that hide paths override it to say why a
    /// path is missing; wrappers must forward it.
    fn lstat(&self, path: &CanonPath) -> Result<Stat, AccessorError> {
        self.maybe_lstat(path)?
            .ok_or_else(|| AccessorError::NotFound(self.show_path(path)))
    }

    /// `pathExists`.
    fn path_exists(&self, path: &CanonPath) -> Result<bool, AccessorError> {
        Ok(self.maybe_lstat(path)?.is_some())
    }

    /// `resolveSymlinks`: follow symlinks in every component (and, with
    /// `resolve_final`, in the last one too), lexically rooted at this
    /// accessor's root. Absolute targets restart from the root. Targets
    /// are bytes and split at `/` bytes, whatever the others are.
    fn resolve_symlinks(
        &self,
        path: &CanonPath,
        resolve_final: bool,
    ) -> Result<CanonPath, AccessorError> {
        let mut res = CanonPath::root();
        let mut links_allowed = 1024;
        let mut todo: std::collections::VecDeque<Vec<u8>> =
            path.components().map(|c| c.to_vec()).collect();
        while let Some(c) = todo.pop_front() {
            if c.is_empty() || c == b"." {
                continue;
            }
            if c == b".." {
                if !res.is_root() {
                    res.pop();
                }
                continue;
            }
            res.push(&c)
                .map_err(|e| AccessorError::Io(e.message_bytes().into()))?;
            if resolve_final || !todo.is_empty() {
                if let Some(st) = self.maybe_lstat(&res)? {
                    if st.file_type == FileType::Symlink {
                        if links_allowed == 0 {
                            return Err(AccessorError::SymlinkLoop(self.show_path(path)));
                        }
                        links_allowed -= 1;
                        let target = self.read_link(&res)?;
                        if target.first() == Some(&b'/') {
                            res = CanonPath::root();
                        } else {
                            res.pop();
                        }
                        for (i, part) in target.split(|&b| b == b'/').enumerate() {
                            todo.insert(i, part.to_vec());
                        }
                    }
                }
            }
        }
        Ok(res)
    }
}

/// An index into an [`AccessorRegistry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccessorId(pub u32);

/// The accessors in use by one evaluation, addressed by [`AccessorId`].
#[derive(Default)]
pub struct AccessorRegistry {
    accessors: Vec<Arc<dyn SourceAccessor>>,
}

impl AccessorRegistry {
    pub fn new() -> AccessorRegistry {
        AccessorRegistry::default()
    }

    pub fn register(&mut self, accessor: Arc<dyn SourceAccessor>) -> AccessorId {
        let id = AccessorId(u32::try_from(self.accessors.len()).expect("too many accessors"));
        self.accessors.push(accessor);
        id
    }

    /// Panics on an id that did not come from this registry.
    pub fn get(&self, id: AccessorId) -> &Arc<dyn SourceAccessor> {
        &self.accessors[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.accessors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accessors.is_empty()
    }
}

/// `SourcePath`: a path inside a particular accessor. This is the owned
/// form; an evaluator can keep `(AccessorId, &[u8])` and rebuild it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourcePath {
    pub accessor: AccessorId,
    pub path: CanonPath,
}

impl SourcePath {
    pub fn new(accessor: AccessorId, path: CanonPath) -> SourcePath {
        SourcePath { accessor, path }
    }

    /// The accessor and path, ready for the trait methods.
    pub fn resolve<'a>(
        &'a self,
        registry: &'a AccessorRegistry,
    ) -> (&'a dyn SourceAccessor, &'a CanonPath) {
        (registry.get(self.accessor).as_ref(), &self.path)
    }

    /// `SourcePath::to_string`, i.e. `showPath`: bytes.
    pub fn display(&self, registry: &AccessorRegistry) -> BString {
        registry.get(self.accessor).show_path(&self.path)
    }

    pub fn parent(&self) -> Option<SourcePath> {
        self.path
            .parent()
            .map(|p| SourcePath::new(self.accessor, p))
    }

    pub fn join(
        &self,
        component: impl AsRef<[u8]>,
    ) -> Result<SourcePath, crate::canon_path::CanonPathError> {
        Ok(SourcePath::new(
            self.accessor,
            self.path.join_component(component)?,
        ))
    }
}

impl fmt::Debug for SourcePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SourcePath({}:{:?})", self.accessor.0, self.path.abs())
    }
}

/// One node of an in-memory tree. Names and targets are bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryNode {
    Regular { executable: bool, contents: Vec<u8> },
    Symlink { target: BString },
    Directory(BTreeMap<BString, MemoryNode>),
}

impl MemoryNode {
    fn stat(&self) -> Stat {
        match self {
            MemoryNode::Regular {
                executable,
                contents,
            } => Stat {
                file_type: FileType::Regular,
                size: Some(contents.len() as u64),
                executable: *executable,
            },
            MemoryNode::Symlink { .. } => Stat {
                file_type: FileType::Symlink,
                size: None,
                executable: false,
            },
            MemoryNode::Directory(_) => Stat {
                file_type: FileType::Directory,
                size: None,
                executable: false,
            },
        }
    }

    fn file_type(&self) -> FileType {
        self.stat().file_type
    }
}

/// `MemorySourceAccessor`: a tree held in memory. Fetched tarballs, NAR
/// contents read back from a daemon, and test fixtures live here.
///
/// The root is created on first insertion; until then the accessor is
/// empty and even `/` does not exist, as in Nix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryAccessor {
    root: Option<MemoryNode>,
    display_prefix: BString,
}

impl MemoryAccessor {
    pub fn new() -> MemoryAccessor {
        MemoryAccessor {
            root: None,
            display_prefix: "«unknown»".into(),
        }
    }

    /// An accessor whose root is `node`.
    pub fn from_root(node: MemoryNode) -> MemoryAccessor {
        MemoryAccessor {
            root: Some(node),
            display_prefix: "«unknown»".into(),
        }
    }

    /// The display prefix: bytes, as a path's are.
    pub fn set_display_prefix(&mut self, prefix: impl AsRef<[u8]>) {
        self.display_prefix = BString::from(prefix.as_ref());
    }

    /// The root node, if the tree is not empty.
    pub fn root(&self) -> Option<&MemoryNode> {
        self.root.as_ref()
    }

    /// Place `node` at `path`, creating missing parent directories. Fails
    /// if a parent exists and is not a directory.
    pub fn insert(&mut self, path: &CanonPath, node: MemoryNode) -> Result<(), AccessorError> {
        if path.is_root() {
            self.root = Some(node);
            return Ok(());
        }
        let shown = self.show_path(path);
        let root = self
            .root
            .get_or_insert_with(|| MemoryNode::Directory(BTreeMap::new()));
        let mut cur = root;
        let comps: Vec<&BStr> = path.components().collect();
        for (i, name) in comps.iter().enumerate() {
            let MemoryNode::Directory(entries) = cur else {
                return Err(AccessorError::NotADirectory(shown));
            };
            if i + 1 == comps.len() {
                entries.insert((*name).to_owned(), node);
                return Ok(());
            }
            cur = entries
                .entry((*name).to_owned())
                .or_insert_with(|| MemoryNode::Directory(BTreeMap::new()));
        }
        unreachable!("non-root path has at least one component")
    }

    pub fn add_file(
        &mut self,
        path: &CanonPath,
        contents: impl Into<Vec<u8>>,
        executable: bool,
    ) -> Result<(), AccessorError> {
        self.insert(
            path,
            MemoryNode::Regular {
                executable,
                contents: contents.into(),
            },
        )
    }

    /// A symlink at `path` to `target`, bytes, as Nix's targets are.
    pub fn add_symlink(
        &mut self,
        path: &CanonPath,
        target: impl AsRef<[u8]>,
    ) -> Result<(), AccessorError> {
        self.insert(
            path,
            MemoryNode::Symlink {
                target: BString::from(target.as_ref()),
            },
        )
    }

    pub fn add_dir(&mut self, path: &CanonPath) -> Result<(), AccessorError> {
        if self.lookup(path).is_some() {
            return Ok(());
        }
        self.insert(path, MemoryNode::Directory(BTreeMap::new()))
    }

    /// The node at `path`, if any. Symlinks along the way are not followed.
    pub fn lookup(&self, path: &CanonPath) -> Option<&MemoryNode> {
        let mut cur = self.root.as_ref()?;
        for name in path.components() {
            match cur {
                MemoryNode::Directory(entries) => cur = entries.get(name)?,
                _ => return None,
            }
        }
        Some(cur)
    }

    /// `MemorySourceAccessor::open` without creating: the node at `path`,
    /// if any. A symlink on the way is an error (`SymlinkNotAllowed`,
    /// naming the symlink); a file on the way means there is nothing.
    fn open(&self, path: &CanonPath) -> Result<Option<&MemoryNode>, AccessorError> {
        let Some(mut cur) = self.root.as_ref() else {
            return Ok(None);
        };
        for (depth, name) in path.components().enumerate() {
            match cur {
                MemoryNode::Directory(entries) => match entries.get(name) {
                    Some(next) => cur = next,
                    None => return Ok(None),
                },
                MemoryNode::Symlink { .. } => {
                    // The path of the symlink: the first `depth` components.
                    let mut link = CanonPath::root();
                    for component in path.components().take(depth) {
                        link.push(component)
                            .expect("components came from a CanonPath");
                    }
                    return Err(AccessorError::IsASymlink(self.show_path(&link)));
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(cur))
    }
}

impl SourceAccessor for MemoryAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        Ok(self.open(path)?.map(MemoryNode::stat))
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        match self.open(path)? {
            None => Err(AccessorError::NotFound(self.show_path(path))),
            Some(MemoryNode::Directory(entries)) => Ok(entries
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.file_type())))
                .collect()),
            Some(MemoryNode::Symlink { .. }) => {
                Err(AccessorError::IsASymlink(self.show_path(path)))
            }
            Some(_) => Err(AccessorError::NotADirectory(self.show_path(path))),
        }
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        match self.open(path)? {
            None => Err(AccessorError::NotFound(self.show_path(path))),
            Some(MemoryNode::Regular { contents, .. }) => Ok(contents.clone()),
            Some(MemoryNode::Symlink { .. }) => {
                Err(AccessorError::IsASymlink(self.show_path(path)))
            }
            Some(_) => Err(AccessorError::NotARegularFile(self.show_path(path))),
        }
    }

    fn read_file_to(&self, path: &CanonPath, out: &mut dyn Write) -> Result<u64, AccessorError> {
        match self.open(path)? {
            None => Err(AccessorError::NotFound(self.show_path(path))),
            Some(MemoryNode::Regular { contents, .. }) => {
                out.write_all(contents)
                    .map_err(|e| AccessorError::Io(e.to_string().into()))?;
                Ok(contents.len() as u64)
            }
            Some(MemoryNode::Symlink { .. }) => {
                Err(AccessorError::IsASymlink(self.show_path(path)))
            }
            Some(_) => Err(AccessorError::NotARegularFile(self.show_path(path))),
        }
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        match self.open(path)? {
            None => Err(AccessorError::NotFound(self.show_path(path))),
            Some(MemoryNode::Symlink { target }) => Ok(target.clone()),
            Some(_) => Err(AccessorError::NotASymlink(self.show_path(path))),
        }
    }

    fn display_prefix(&self) -> &BStr {
        self.display_prefix.as_bstr()
    }
}

/// `MountedSourceAccessor`: accessors grafted onto paths of one tree. A
/// root mount is required; lookups go to the nearest mount point above
/// the path.
pub struct MountedAccessor {
    mounts: BTreeMap<CanonPath, Arc<dyn SourceAccessor>>,
}

impl MountedAccessor {
    pub fn new(root: Arc<dyn SourceAccessor>) -> MountedAccessor {
        let mut mounts = BTreeMap::new();
        mounts.insert(CanonPath::root(), root);
        MountedAccessor { mounts }
    }

    pub fn mount(&mut self, mount_point: CanonPath, accessor: Arc<dyn SourceAccessor>) {
        self.mounts.insert(mount_point, accessor);
    }

    /// `resolve`: the accessor responsible for `path` and the path within
    /// it. The mounts are looked up by the bytes of `path`'s ancestors,
    /// nearest first, without building a path for each.
    pub fn resolve(&self, path: &CanonPath) -> (&Arc<dyn SourceAccessor>, CanonPath) {
        for mount_point in path.ancestors() {
            if let Some(acc) = self.mounts.get(mount_point) {
                let rel = if mount_point.len() == 1 {
                    // The root mount: the path as it is.
                    path.clone()
                } else if mount_point.len() == path.as_bytes().len() {
                    CanonPath::root()
                } else {
                    // The rest starts at the slash after the mount point.
                    CanonPath::unchecked(path.as_bytes()[mount_point.len()..].to_vec())
                };
                return (acc, rel);
            }
        }
        unreachable!("invariant: the root is always mounted")
    }
}

impl SourceAccessor for MountedAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.maybe_lstat(&sub)
    }

    fn lstat(&self, path: &CanonPath) -> Result<Stat, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.lstat(&sub)
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.read_dir(&sub)
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.read_file(&sub)
    }

    fn read_file_to(&self, path: &CanonPath, out: &mut dyn Write) -> Result<u64, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.read_file_to(&sub, out)
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.read_link(&sub)
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new("")
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        let (acc, sub) = self.resolve(path);
        acc.show_path(&sub)
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        let (acc, sub) = self.resolve(path);
        acc.physical_path(&sub)
    }

    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(Hash, u64)> {
        let (acc, sub) = self.resolve(path);
        acc.precomputed_nar_hash(&sub)
    }

    fn remembered_nar_hash(&self, path: &CanonPath) -> Result<Option<Hash>, AccessorError> {
        let (acc, sub) = self.resolve(path);
        acc.remembered_nar_hash(&sub)
    }

    fn mounted_show_path(&self, path: &CanonPath) -> Option<BString> {
        let (acc, sub) = self.resolve(path);
        let root = self
            .mounts
            .get(&CanonPath::root())
            .expect("invariant: the root is always mounted");
        if Arc::ptr_eq(acc, root) {
            acc.mounted_show_path(&sub)
        } else {
            Some(acc.show_path(&sub))
        }
    }
}

#[cfg(feature = "std-fs")]
pub use fs::FsAccessor;

#[cfg(feature = "std-fs")]
mod fs {
    use super::*;
    use crate::os_path;
    use std::io::Read;
    use std::path::{Path, PathBuf};

    /// `PosixSourceAccessor`: a subtree of the real file system. Paths are
    /// resolved relative to `root`; the root accessor of an evaluation is
    /// `FsAccessor::new("/")`, whose `show_path` is the plain path.
    ///
    /// Names and symlink targets are bytes, as the operating system gives
    /// them (on Unix, any bytes; Nix's `readDirectory` takes `d_name` as
    /// it is). Elsewhere a name that is not Unicode cannot be read
    /// ([`AccessorError::NonUtf8`]), nor a path that is not UTF-8 found.
    ///
    /// As the trait requires, reading a file or directory through a final
    /// symlink is refused, with Nix's "path '…' is a symlink": a file is
    /// opened with `O_NOFOLLOW`, as Nix opens it, and a directory with
    /// `O_DIRECTORY | O_NOFOLLOW` and listed through that descriptor
    /// (where Nix checks with `lstat` and then lists the path). Unlike Nix, the operating
    /// system follows symlinks among the ancestors (and `root` itself),
    /// where Nix would refuse them; callers that resolved their paths have
    /// none there.
    pub struct FsAccessor {
        root: PathBuf,
        display_prefix: BString,
    }

    impl FsAccessor {
        pub fn new(root: impl Into<PathBuf>) -> FsAccessor {
            let root = root.into();
            let display_prefix = if root == Path::new("/") {
                BString::default()
            } else {
                match os_path::path_bytes(&root) {
                    Some(bytes) => BString::from(bytes),
                    // Not Unix, and the root is not Unicode: it is only
                    // shown, so shown as best it can be.
                    None => BString::from(root.to_string_lossy().into_owned()),
                }
            };
            FsAccessor {
                root,
                display_prefix,
            }
        }

        pub fn root(&self) -> &Path {
            &self.root
        }

        /// Where `path` is on the file system. On Unix every path is;
        /// elsewhere one that is not UTF-8 cannot be named.
        fn to_fs(&self, path: &CanonPath) -> Result<PathBuf, AccessorError> {
            if path.is_root() {
                return Ok(self.root.clone());
            }
            match os_path::to_path(path.rel()) {
                Some(rel) => Ok(self.root.join(rel)),
                None => Err(AccessorError::NonUtf8(self.show_path(path))),
            }
        }

        fn io_err(&self, path: &CanonPath, what: &str, e: std::io::Error) -> AccessorError {
            match e.kind() {
                std::io::ErrorKind::NotFound => AccessorError::NotFound(self.show_path(path)),
                std::io::ErrorKind::NotADirectory => {
                    AccessorError::NotADirectory(self.show_path(path))
                }
                std::io::ErrorKind::IsADirectory => {
                    AccessorError::NotARegularFile(self.show_path(path))
                }
                _ => AccessorError::Io(
                    [
                        what.as_bytes(),
                        b" '",
                        &self.show_path(path),
                        b"': ",
                        e.to_string().as_bytes(),
                    ]
                    .concat()
                    .into(),
                ),
            }
        }

        /// The metadata of what `path` names, refusing a final symlink. The
        /// root is the exception: it is opened through the operating
        /// system, which follows it. Only where there is no `O_NOFOLLOW`
        /// to refuse the symlink at the open.
        #[cfg(not(unix))]
        fn stat_no_final_symlink(
            &self,
            path: &CanonPath,
            fs_path: &Path,
        ) -> Result<std::fs::Metadata, AccessorError> {
            let md = if path.is_root() {
                std::fs::metadata(fs_path)
            } else {
                std::fs::symlink_metadata(fs_path)
            }
            .map_err(|e| self.io_err(path, "getting status of", e))?;
            if md.file_type().is_symlink() {
                return Err(AccessorError::PathIsASymlink(self.show_path(path)));
            }
            Ok(md)
        }

        /// The file `path` names, opened for reading. A final symlink is
        /// refused by the open itself (`O_NOFOLLOW`, as Nix opens it), so
        /// nothing can replace the file with a symlink between a check
        /// and the read; the root is the exception, opened through the
        /// operating system, which follows it. A directory is not a
        /// regular file. Also returns the file's size.
        fn open_file(&self, path: &CanonPath) -> Result<(std::fs::File, u64), AccessorError> {
            let fs_path = self.to_fs(path)?;
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            if !path.is_root() {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            #[cfg(not(unix))]
            self.stat_no_final_symlink(path, &fs_path)?;
            let file = match options.open(&fs_path) {
                Ok(file) => file,
                #[cfg(unix)]
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) && !path.is_root() => {
                    return Err(AccessorError::PathIsASymlink(self.show_path(path)))
                }
                Err(e) => return Err(self.io_err(path, "opening", e)),
            };
            let md = file
                .metadata()
                .map_err(|e| self.io_err(path, "getting status of", e))?;
            if md.is_dir() {
                return Err(AccessorError::NotARegularFile(self.show_path(path)));
            }
            Ok((file, md.len()))
        }

        fn file_type_of(ft: std::fs::FileType) -> FileType {
            if ft.is_symlink() {
                FileType::Symlink
            } else if ft.is_dir() {
                FileType::Directory
            } else if ft.is_file() {
                FileType::Regular
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_char_device() {
                        return FileType::Char;
                    } else if ft.is_block_device() {
                        return FileType::Block;
                    } else if ft.is_socket() {
                        return FileType::Socket;
                    } else if ft.is_fifo() {
                        return FileType::Fifo;
                    }
                }
                FileType::Unknown
            }
        }
    }

    #[cfg(unix)]
    fn file_type_of_rustix(file_type: rustix::fs::FileType) -> FileType {
        use rustix::fs::FileType as F;
        match file_type {
            F::RegularFile => FileType::Regular,
            F::Directory => FileType::Directory,
            F::Symlink => FileType::Symlink,
            F::Fifo => FileType::Fifo,
            F::Socket => FileType::Socket,
            F::CharacterDevice => FileType::Char,
            F::BlockDevice => FileType::Block,
            F::Unknown => FileType::Unknown,
        }
    }

    impl SourceAccessor for FsAccessor {
        fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
            match std::fs::symlink_metadata(self.to_fs(path)?) {
                Ok(md) => {
                    let file_type = Self::file_type_of(md.file_type());
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        file_type == FileType::Regular && md.permissions().mode() & 0o100 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    Ok(Some(Stat {
                        file_type,
                        size: (file_type == FileType::Regular).then_some(md.len()),
                        executable,
                    }))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                // A component that is not a directory also means "nothing here".
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
                Err(e) => Err(self.io_err(path, "getting status of", e)),
            }
        }

        /// The directory is opened with `O_DIRECTORY | O_NOFOLLOW` (the
        /// root without `O_NOFOLLOW`) and listed through that descriptor,
        /// so a final symlink is refused by the open itself and nothing
        /// can put one in the directory's place between a check and the
        /// listing. Names are taken as the bytes `d_name` holds, as in
        /// `PosixSourceAccessor::readDirectory`. An entry whose type the
        /// directory does not record is looked at with `fstatat` (no type
        /// when that is not permitted, as in Nix).
        #[cfg(unix)]
        fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
            use rustix::fs::{AtFlags, Dir, Mode, OFlags};
            use rustix::io::Errno;
            let fs_path = self.to_fs(path)?;
            let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            if !path.is_root() {
                flags |= OFlags::NOFOLLOW;
            }
            let fd = match rustix::fs::open(&fs_path, flags, Mode::empty()) {
                Ok(fd) => fd,
                // A symlink, or not a directory: which one, for the
                // message only (nothing was opened through the link).
                Err(Errno::LOOP | Errno::NOTDIR) if !path.is_root() => {
                    return Err(match std::fs::symlink_metadata(&fs_path) {
                        Ok(md) if md.file_type().is_symlink() => {
                            AccessorError::PathIsASymlink(self.show_path(path))
                        }
                        _ => AccessorError::NotADirectory(self.show_path(path)),
                    });
                }
                Err(e) => return Err(self.io_err(path, "opening directory", e.into())),
            };
            let mut dir =
                Dir::new(fd).map_err(|e| self.io_err(path, "opening directory", e.into()))?;
            let mut entries = DirEntries::new();
            while let Some(entry) = dir.next() {
                let entry = entry.map_err(|e| self.io_err(path, "reading directory", e.into()))?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let file_type = match entry.file_type() {
                    rustix::fs::FileType::Unknown => {
                        let dir_fd = dir
                            .fd()
                            .map_err(|e| self.io_err(path, "reading directory", e.into()))?;
                        match rustix::fs::statat(
                            dir_fd,
                            entry.file_name(),
                            AtFlags::SYMLINK_NOFOLLOW,
                        ) {
                            Ok(st) => Some(rustix::fs::FileType::from_raw_mode(st.st_mode)),
                            Err(Errno::ACCESS | Errno::PERM) => None,
                            Err(e) => {
                                let entry_path = path
                                    .join_component(name)
                                    .expect("invariant: a directory entry's name is one component");
                                return Err(self.io_err(
                                    &entry_path,
                                    "getting status of",
                                    e.into(),
                                ));
                            }
                        }
                    }
                    file_type => Some(file_type),
                };
                entries.insert(BString::from(name), file_type.map(file_type_of_rustix));
            }
            Ok(entries)
        }

        #[cfg(not(unix))]
        fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
            let fs_path = self.to_fs(path)?;
            if !self.stat_no_final_symlink(path, &fs_path)?.is_dir() {
                return Err(AccessorError::NotADirectory(self.show_path(path)));
            }
            let mut entries = DirEntries::new();
            let it = std::fs::read_dir(&fs_path)
                .map_err(|e| self.io_err(path, "opening directory", e))?;
            for entry in it {
                let entry = entry.map_err(|e| self.io_err(path, "reading directory", e))?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| AccessorError::NonUtf8(self.show_path(path)))?;
                let ft = match entry.file_type() {
                    Ok(ft) => Some(Self::file_type_of(ft)),
                    // As on Unix: no type when it cannot be looked at.
                    Err(_) => None,
                };
                entries.insert(BString::from(name), ft);
            }
            Ok(entries)
        }

        fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
            let (mut file, size) = self.open_file(path)?;
            // The size is a hint, as for `std::fs::read`: the file may change.
            let mut contents = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            file.read_to_end(&mut contents)
                .map_err(|e| self.io_err(path, "reading", e))?;
            Ok(contents)
        }

        fn read_file_to(
            &self,
            path: &CanonPath,
            out: &mut dyn Write,
        ) -> Result<u64, AccessorError> {
            let (mut file, _) = self.open_file(path)?;
            std::io::copy(&mut file, out).map_err(|e| self.io_err(path, "reading", e))
        }

        /// The target's bytes, as `readlink` gives them.
        fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
            let target = std::fs::read_link(self.to_fs(path)?).map_err(|e| {
                if e.kind() == std::io::ErrorKind::InvalidInput {
                    AccessorError::NotASymlink(self.show_path(path))
                } else {
                    self.io_err(path, "reading symlink", e)
                }
            })?;
            match os_path::path_bytes(&target) {
                Some(bytes) => Ok(BString::from(bytes)),
                None => Err(AccessorError::NonUtf8(self.show_path(path))),
            }
        }

        fn display_prefix(&self) -> &BStr {
            self.display_prefix.as_bstr()
        }

        /// `None` for a path the operating system cannot name (not Unix,
        /// and not UTF-8): it is nowhere on this file system.
        fn physical_path(&self, path: &CanonPath) -> Option<PathBuf> {
            if path.is_root() {
                Some(self.root.clone())
            } else {
                os_path::to_path(path.rel()).map(|relative| self.root.join(relative))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn sample() -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        m.add_file(&p("/a/b.txt"), "hello", false).unwrap();
        m.add_file(&p("/a/run.sh"), "#!/bin/sh", true).unwrap();
        m.add_symlink(&p("/a/link"), "b.txt").unwrap();
        m.add_symlink(&p("/abs"), "/a/b.txt").unwrap();
        m.add_dir(&p("/empty")).unwrap();
        m
    }

    #[test]
    fn memory_accessor_basics() {
        let m = sample();
        assert_eq!(m.read_file(&p("/a/b.txt")).unwrap(), b"hello");
        assert!(m.lstat(&p("/a/run.sh")).unwrap().executable);
        assert_eq!(m.lstat(&p("/a")).unwrap().file_type, FileType::Directory);
        assert_eq!(m.read_link(&p("/a/link")).unwrap(), "b.txt");
        assert_eq!(
            m.read_dir(&p("/a"))
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["b.txt", "link", "run.sh"]
        );
        assert!(m.maybe_lstat(&p("/nope")).unwrap().is_none());
        assert_eq!(
            m.read_file(&p("/nope")).unwrap_err().to_string(),
            "path '«unknown»/nope' does not exist"
        );
        assert_eq!(
            m.read_dir(&p("/a/b.txt")).unwrap_err().to_string(),
            "file '«unknown»/a/b.txt' is not a directory"
        );
        assert_eq!(
            m.read_link(&p("/a/b.txt")).unwrap_err().to_string(),
            "file '«unknown»/a/b.txt' is not a symbolic link"
        );
        assert!(MemoryAccessor::new()
            .maybe_lstat(&CanonPath::root())
            .unwrap()
            .is_none());
    }

    /// Names, targets, the display prefix and the messages are bytes:
    /// listed in unsigned byte order (`z` before `\xe9` and `\xff`), and
    /// shown as they are.
    #[test]
    fn memory_accessor_names_are_bytes() {
        let mut m = MemoryAccessor::new();
        m.set_display_prefix(b"/nix/store/x-d\xe9");
        for name in [&b"z"[..], b"caf\xe9", b"\xff", b"a\x80b"] {
            m.add_file(
                &CanonPath::root().join_component(name).unwrap(),
                name,
                false,
            )
            .unwrap();
        }
        m.add_symlink(&p(b"/l\xff"), b"t\xe9").unwrap();
        assert_eq!(
            m.read_dir(&CanonPath::root())
                .unwrap()
                .keys()
                .map(|k| k.as_slice())
                .collect::<Vec<_>>(),
            [&b"a\x80b"[..], b"caf\xe9", b"l\xff", b"z", b"\xff"]
        );
        assert_eq!(m.read_file(&p(b"/caf\xe9")).unwrap(), b"caf\xe9");
        assert_eq!(m.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9".as_slice());
        assert_eq!(
            m.show_path(&p(b"/caf\xe9")),
            b"/nix/store/x-d\xe9/caf\xe9".as_slice()
        );
        let err = m.read_file(&p(b"/d\xe9")).unwrap_err();
        assert_eq!(
            err.message_bytes(),
            b"path '/nix/store/x-d\xe9/d\xe9' does not exist"
        );
        assert_eq!(
            err.to_string(),
            "path '/nix/store/x-d\u{fffd}/d\u{fffd}' does not exist"
        );
        assert_eq!(
            m.resolve_symlinks(&p(b"/l\xff"), true).unwrap(),
            p(b"/t\xe9")
        );
    }

    #[test]
    fn messages_keep_their_bytes() {
        let shown = BString::from(&b"/a\xff"[..]);
        for (err, expected) in [
            (
                AccessorError::NotFound(shown.clone()),
                &b"path '/a\xff' does not exist"[..],
            ),
            (
                AccessorError::NotADirectory(shown.clone()),
                b"file '/a\xff' is not a directory",
            ),
            (
                AccessorError::NotARegularFile(shown.clone()),
                b"file '/a\xff' is not a regular file",
            ),
            (
                AccessorError::NotASymlink(shown.clone()),
                b"file '/a\xff' is not a symbolic link",
            ),
            (
                AccessorError::IsASymlink(shown.clone()),
                b"file '/a\xff' is a symlink",
            ),
            (
                AccessorError::PathIsASymlink(shown.clone()),
                b"path '/a\xff' is a symlink",
            ),
            (
                AccessorError::SymlinkLoop(shown.clone()),
                b"infinite symlink recursion in path '/a\xff'",
            ),
            (
                AccessorError::MissingParent(shown.clone()),
                b"file '/a\xff' cannot be created because some parent directories don't exist",
            ),
            (
                AccessorError::NonUtf8(shown.clone()),
                b"file name or symlink target in '/a\xff' is not valid UTF-8",
            ),
            (AccessorError::Restricted(shown.clone()), b"/a\xff"),
            (AccessorError::Io(shown.clone()), b"/a\xff"),
            (
                AccessorError::traced(AccessorError::NotFound(shown.clone()), "while x"),
                b"path '/a\xff' does not exist",
            ),
        ] {
            assert_eq!(BStr::new(&err.message_bytes()), BStr::new(expected));
            // `Display` says the same, lossily.
            assert_eq!(
                err.to_string(),
                String::from_utf8_lossy(expected),
                "{err:?}"
            );
        }
    }

    /// `MemorySourceAccessor::open`: a symlink on the way to a path is
    /// `SymlinkNotAllowed` naming the symlink, for every operation; a
    /// regular file on the way means there is nothing there.
    #[test]
    fn memory_accessor_refuses_symlinks_on_the_way() {
        let mut m = sample();
        m.add_symlink(&p("/dirlink"), "a").unwrap();
        let refused = "file '«unknown»/dirlink' is a symlink";
        assert_eq!(
            m.read_file(&p("/dirlink/b.txt")).unwrap_err().to_string(),
            refused
        );
        assert_eq!(
            m.maybe_lstat(&p("/dirlink/b.txt")).unwrap_err().to_string(),
            refused
        );
        assert_eq!(
            m.read_dir(&p("/dirlink/x/y")).unwrap_err().to_string(),
            refused
        );
        assert_eq!(
            m.read_link(&p("/a/link/x")).unwrap_err().to_string(),
            "file '«unknown»/a/link' is a symlink"
        );
        assert!(m.maybe_lstat(&p("/a/b.txt/x")).unwrap().is_none());
        assert_eq!(
            m.read_file(&p("/a/b.txt/x")).unwrap_err().to_string(),
            "path '«unknown»/a/b.txt/x' does not exist"
        );
    }

    #[test]
    fn resolves_symlinks() {
        let m = sample();
        assert_eq!(
            m.resolve_symlinks(&p("/a/link"), true).unwrap().abs(),
            "/a/b.txt"
        );
        assert_eq!(
            m.resolve_symlinks(&p("/a/link"), false).unwrap().abs(),
            "/a/link"
        );
        assert_eq!(
            m.resolve_symlinks(&p("/abs"), true).unwrap().abs(),
            "/a/b.txt"
        );
        let mut l = MemoryAccessor::new();
        l.add_symlink(&p("/x"), "y").unwrap();
        l.add_symlink(&p("/y"), "x").unwrap();
        assert!(l.resolve_symlinks(&p("/x"), true).is_err());
    }

    #[test]
    fn mounted_accessor_routes_to_mounts() {
        let root = Arc::new(sample());
        let mut inner = MemoryAccessor::new();
        inner.add_file(&p("/f"), "inner", false).unwrap();
        inner.set_display_prefix("/nix/store/xxx-src");
        let mut m = MountedAccessor::new(root);
        m.mount(p("/nix/store/xxx-src"), Arc::new(inner));
        assert_eq!(m.read_file(&p("/nix/store/xxx-src/f")).unwrap(), b"inner");
        assert_eq!(m.read_file(&p("/a/b.txt")).unwrap(), b"hello");
        assert_eq!(
            m.show_path(&p("/nix/store/xxx-src/f")),
            "/nix/store/xxx-src/f"
        );
        let (_, sub) = m.resolve(&p("/nix/store/xxx-src/d/f"));
        assert_eq!(sub.abs(), "/d/f");
        let (_, sub) = m.resolve(&p("/nix/store/xxx-src"));
        assert_eq!(sub.abs(), "/");
        let (_, sub) = m.resolve(&p("/a/b.txt"));
        assert_eq!(sub.abs(), "/a/b.txt");
        // A path that only starts with a mount point's name is not in it.
        let (_, sub) = m.resolve(&p("/nix/store/xxx-srcx"));
        assert_eq!(sub.abs(), "/nix/store/xxx-srcx");
        let mut bytes = MemoryAccessor::new();
        bytes.add_file(&p(b"/\xe9"), "e", false).unwrap();
        m.mount(p(b"/m\xff"), Arc::new(bytes));
        assert_eq!(m.read_file(&p(b"/m\xff/\xe9")).unwrap(), b"e");
        assert_eq!(m.resolve(&p(b"/m\xff/\xe9")).1, p(b"/\xe9"));
    }

    #[test]
    fn registry_and_source_paths() {
        let mut reg = AccessorRegistry::new();
        let id = reg.register(Arc::new(sample()));
        let sp = SourcePath::new(id, p("/a/b.txt"));
        let (acc, path) = sp.resolve(&reg);
        assert_eq!(acc.read_file(path).unwrap(), b"hello");
        assert_eq!(sp.display(&reg), "«unknown»/a/b.txt");
        assert_eq!(sp.parent().unwrap().path.abs(), "/a");
        assert_eq!(sp.join(b"\xe9").unwrap().path.abs(), b"/a/b.txt/\xe9");
    }

    #[cfg(feature = "std-fs")]
    #[test]
    fn fs_accessor_reads_a_tempdir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/f"), "data").unwrap();
        std::fs::write(dir.path().join("x"), "exe").unwrap();
        std::fs::set_permissions(dir.path().join("x"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("sub/f", dir.path().join("l")).unwrap();
        let fs = FsAccessor::new(dir.path());
        assert_eq!(fs.read_file(&p("/sub/f")).unwrap(), b"data");
        assert!(fs.lstat(&p("/x")).unwrap().executable);
        assert!(!fs.lstat(&p("/sub/f")).unwrap().executable);
        assert_eq!(fs.read_link(&p("/l")).unwrap(), "sub/f");
        assert_eq!(fs.lstat(&p("/l")).unwrap().file_type, FileType::Symlink);
        assert_eq!(fs.read_dir(&CanonPath::root()).unwrap().len(), 3);
        assert!(fs.maybe_lstat(&p("/missing")).unwrap().is_none());
        assert!(fs.maybe_lstat(&p("/x/below-a-file")).unwrap().is_none());
        assert!(fs.read_dir(&p("/x")).is_err());
        assert!(fs.read_link(&p("/x")).is_err());
        let root = FsAccessor::new("/");
        assert_eq!(root.show_path(&p("/etc")), "/etc");
        // The open refuses the symlink itself (`O_NOFOLLOW`), and a
        // directory is not a file.
        let shown = |rel: &str| format!("{}{rel}", dir.path().display());
        assert_eq!(
            fs.read_file(&p("/l")).unwrap_err().to_string(),
            format!("path '{}' is a symlink", shown("/l"))
        );
        assert_eq!(
            fs.read_file_to(&p("/l"), &mut Vec::new())
                .unwrap_err()
                .to_string(),
            format!("path '{}' is a symlink", shown("/l"))
        );
        assert_eq!(
            fs.read_file(&p("/sub")).unwrap_err().to_string(),
            format!("file '{}' is not a regular file", shown("/sub"))
        );
        assert_eq!(
            fs.read_file(&p("/missing")).unwrap_err().to_string(),
            format!("path '{}' does not exist", shown("/missing"))
        );
        // The root itself is opened through the operating system.
        let file_root = FsAccessor::new(dir.path().join("l"));
        assert_eq!(file_root.read_file(&CanonPath::root()).unwrap(), b"data");
    }

    /// Names and targets are the bytes the file system holds (Nix's
    /// `PosixSourceAccessor`), in unsigned byte order, and the root's
    /// path is shown as its bytes.
    #[cfg(all(feature = "std-fs", unix))]
    #[test]
    fn fs_accessor_names_are_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let os = |b: &[u8]| std::ffi::OsStr::from_bytes(b).to_owned();
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join(os(b"d\xe9"));
        std::fs::create_dir_all(dir.join(os(b"sub\xe9"))).unwrap();
        for name in [&b"z"[..], b"caf\xe9", b"\xff", b"a\x80b", b"sub\xe9/f"] {
            std::fs::write(dir.join(os(name)), name).unwrap();
        }
        std::os::unix::fs::symlink(os(b"t\xe9"), dir.join(os(b"l\xff"))).unwrap();
        let fs = FsAccessor::new(&dir);
        let listing = fs.read_dir(&CanonPath::root()).unwrap();
        assert_eq!(
            listing.keys().map(|k| k.as_slice()).collect::<Vec<_>>(),
            [
                &b"a\x80b"[..],
                b"caf\xe9",
                b"l\xff",
                b"sub\xe9",
                b"z",
                b"\xff"
            ]
        );
        assert_eq!(listing[BStr::new(b"l\xff")], Some(FileType::Symlink));
        assert_eq!(listing[BStr::new(b"sub\xe9")], Some(FileType::Directory));
        assert_eq!(fs.read_file(&p(b"/caf\xe9")).unwrap(), b"caf\xe9");
        assert_eq!(fs.read_file(&p(b"/sub\xe9/f")).unwrap(), b"sub\xe9/f");
        assert_eq!(fs.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9".as_slice());
        let dir_bytes = dir.as_os_str().as_bytes();
        assert_eq!(fs.display_prefix(), BStr::new(dir_bytes));
        assert_eq!(
            fs.show_path(&p(b"/caf\xe9")),
            [dir_bytes, b"/caf\xe9"].concat().as_slice()
        );
        assert_eq!(
            fs.read_file(&p(b"/missing\xe9"))
                .unwrap_err()
                .message_bytes(),
            [&b"path '"[..], dir_bytes, b"/missing\xe9' does not exist"].concat()
        );
        assert_eq!(
            fs.physical_path(&p(b"/caf\xe9")),
            Some(dir.join(os(b"caf\xe9")))
        );
    }

    /// The tree of [`conformance`], to be built by each implementation.
    const CONFORMANCE_FILES: [(&str, &str); 2] = [("/a/b.txt", "hello"), ("/a/c.txt", "")];
    const CONFORMANCE_LINKS: [(&str, &str); 3] = [
        ("/a/link", "b.txt"),
        ("/abs", "/a/b.txt"),
        ("/dirlink", "a"),
    ];

    /// What the trait promises, whatever the implementation: plain
    /// reads, and a final symlink refused by every read that is not about
    /// the symlink itself, until the caller resolves it.
    fn conformance(acc: &dyn SourceAccessor) {
        let refused = |r: Result<(), AccessorError>, what: &str| match r {
            Err(AccessorError::IsASymlink(_) | AccessorError::PathIsASymlink(_)) => {}
            other => panic!("{what}: expected a symlink to be refused, got {other:?}"),
        };
        let mut sink = Vec::new();
        for link in ["/a/link", "/abs"] {
            refused(acc.read_file(&p(link)).map(drop), link);
            refused(acc.read_file_to(&p(link), &mut sink).map(drop), link);
            assert_eq!(acc.lstat(&p(link)).unwrap().file_type, FileType::Symlink);
            let resolved = acc.resolve_symlinks(&p(link), true).unwrap();
            assert_eq!(resolved.abs(), "/a/b.txt");
            assert_eq!(acc.read_file(&resolved).unwrap(), b"hello");
        }
        refused(acc.read_dir(&p("/dirlink")).map(drop), "/dirlink");
        let resolved = acc.resolve_symlinks(&p("/dirlink"), true).unwrap();
        assert_eq!(resolved.abs(), "/a");
        assert_eq!(
            acc.read_dir(&resolved).unwrap(),
            DirEntries::from([
                ("b.txt".into(), Some(FileType::Regular)),
                ("c.txt".into(), Some(FileType::Regular)),
                ("link".into(), Some(FileType::Symlink)),
            ])
        );
        assert_eq!(
            acc.resolve_symlinks(&p("/dirlink/link"), true)
                .unwrap()
                .abs(),
            "/a/b.txt"
        );
        assert!(sink.is_empty());
        assert_eq!(acc.read_file_to(&p("/a/b.txt"), &mut sink).unwrap(), 5);
        assert_eq!(sink, b"hello");
        assert_eq!(acc.read_file(&p("/a/c.txt")).unwrap(), b"");
        assert_eq!(acc.read_link(&p("/a/link")).unwrap(), "b.txt");
        assert!(matches!(
            acc.read_file(&p("/a")),
            Err(AccessorError::NotARegularFile(_))
        ));
        assert!(matches!(
            acc.read_dir(&p("/a/b.txt")),
            Err(AccessorError::NotADirectory(_))
        ));
        assert!(matches!(
            acc.read_link(&p("/a/b.txt")),
            Err(AccessorError::NotASymlink(_))
        ));
        assert!(matches!(
            acc.read_file(&p("/missing")),
            Err(AccessorError::NotFound(_))
        ));
    }

    #[test]
    fn memory_accessor_conforms() {
        let mut m = MemoryAccessor::new();
        for (path, contents) in CONFORMANCE_FILES {
            m.add_file(&p(path), contents, false).unwrap();
        }
        for (path, target) in CONFORMANCE_LINKS {
            m.add_symlink(&p(path), target).unwrap();
        }
        conformance(&m);
    }

    #[cfg(feature = "std-fs")]
    #[test]
    fn fs_accessor_conforms() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        for (path, contents) in CONFORMANCE_FILES {
            std::fs::write(dir.path().join(&path[1..]), contents).unwrap();
        }
        for (path, target) in CONFORMANCE_LINKS {
            std::os::unix::fs::symlink(target, dir.path().join(&path[1..])).unwrap();
        }
        let fs = FsAccessor::new(dir.path());
        conformance(&fs);
        // With Nix's message, and the accessor's root may itself be a
        // symlink, which the operating system follows.
        assert_eq!(
            fs.read_file(&p("/a/link")).unwrap_err().to_string(),
            format!("path '{}/a/link' is a symlink", dir.path().display())
        );
        let through_link = FsAccessor::new(dir.path().join("dirlink"));
        assert_eq!(through_link.read_file(&p("/b.txt")).unwrap(), b"hello");
        assert_eq!(through_link.read_dir(&CanonPath::root()).unwrap().len(), 3);
        // A directory is opened without following a final symlink, to a
        // directory or to a file, and listed through that descriptor.
        for link in ["/dirlink", "/a/link"] {
            assert_eq!(
                fs.read_dir(&p(link)).unwrap_err().to_string(),
                format!("path '{}{link}' is a symlink", dir.path().display())
            );
        }
        assert_eq!(
            fs.read_dir(&p("/a/b.txt/c")).unwrap_err().to_string(),
            format!(
                "file '{}/a/b.txt/c' is not a directory",
                dir.path().display()
            )
        );
        assert!(matches!(
            fs.read_dir(&p("/missing")),
            Err(AccessorError::NotFound(_))
        ));
    }
}
