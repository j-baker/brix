//! The root file system pure evaluation reads (Nix's `storeFS` under the
//! allow list): the store directory, checked with the store, plus the
//! directories the configuration allows, with fetched trees and built
//! outputs mounted on top.
//!
//! Path names are bytes, as in Nix: a directory or a file whose name is
//! not UTF-8 is read and shown as it is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use brix_store::{
    AccessorError, CanonPath, DirEntries, Hash, MemoryAccessor, MountedAccessor, SourceAccessor,
    Stat, StoreDir, StorePath,
};
use bstr::{BStr, BString};

use crate::store::store_has;

/// The root file system of pure evaluation (`storeFS` in Nix): empty,
/// with the real store directory mounted at its path, plus whatever the
/// configuration allows. Built outputs are mounted here after a build.
pub(crate) struct HostRoot {
    inner: RwLock<MountedAccessor>,
}

impl HostRoot {
    /// `allowed` are directories of the real file system (they come from
    /// file-system paths, see `NativeHost::new`).
    pub(crate) fn new(store_dir: &StoreDir, allowed: &[CanonPath], store_fs: StoreFs) -> HostRoot {
        let mut mounted = MountedAccessor::new(Arc::new(MemoryAccessor::new()));
        let store = CanonPath::new(store_dir.as_str()).expect("store dir is absolute");
        mounted.mount(store.clone(), Arc::new(store_fs));
        for p in allowed {
            let dir = brix_store::os_path::to_path(p.as_bytes())
                .expect("invariant: an allowed path was made from a file-system path");
            mounted.mount(
                p.clone(),
                Arc::new(LogicalFs::new(dir.to_path_buf(), p.abs())),
            );
        }
        HostRoot {
            inner: RwLock::new(mounted),
        }
    }

    pub(crate) fn mount(&self, at: CanonPath, accessor: Arc<dyn SourceAccessor>) {
        self.inner
            .write()
            .expect("host root poisoned")
            .mount(at, accessor);
    }

    fn with<R>(&self, f: impl FnOnce(&MountedAccessor) -> R) -> R {
        f(&self.inner.read().expect("host root poisoned"))
    }
}

impl SourceAccessor for HostRoot {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        self.with(|m| m.maybe_lstat(path))
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        self.with(|m| m.lstat(path))
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.with(|m| m.read_dir(path))
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        self.with(|m| m.read_file(path))
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        self.with(|m| m.read_link(path))
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        self.with(|m| m.show_path(path))
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        self.with(|m| m.physical_path(path))
    }

    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(Hash, u64)> {
        self.with(|m| m.precomputed_nar_hash(path))
    }

    fn remembered_nar_hash(
        &self,
        path: &CanonPath,
    ) -> std::result::Result<Option<Hash>, AccessorError> {
        self.with(|m| m.remembered_nar_hash(path))
    }
}

/// `prefix` followed by `path` the way an accessor with a display prefix
/// shows it (`showPath`): the root as nothing, any other path as its
/// absolute form. Bytes, as both are.
fn prefixed(prefix: &[u8], path: &CanonPath) -> BString {
    let rest: &[u8] = path.abs_or_empty();
    let mut shown = Vec::with_capacity(prefix.len() + rest.len());
    shown.extend_from_slice(prefix);
    shown.extend_from_slice(rest);
    shown.into()
}

/// A directory of the real file system shown at its own absolute path
/// (an `FsAccessor` shows paths relative to its root).
struct LogicalFs {
    inner: brix_store::FsAccessor,
    prefix: BString,
}

impl LogicalFs {
    /// The directory `dir`, its paths shown under `prefix`.
    fn new(dir: PathBuf, prefix: impl Into<BString>) -> LogicalFs {
        LogicalFs {
            inner: brix_store::FsAccessor::new(dir),
            prefix: prefix.into(),
        }
    }
}

impl SourceAccessor for LogicalFs {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        self.inner.maybe_lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        self.inner.read_file(path)
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        self.inner.read_link(path)
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new(&self.prefix)
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        prefixed(&self.prefix, path)
    }

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        self.inner.physical_path(path)
    }
}

/// The store directory as pure evaluation sees it: Nix mounts
/// `store->getFSAccessor(requireValidPath = true)` there, so a store path
/// the store does not know is `path '<store path>' is not valid` on any
/// access, however the file system looks (`src/libexpr/eval.cc`,
/// `src/libstore/store-api.cc`). Trees brix fetched are mounted above
/// this accessor and never reach it; everything else is checked with the
/// daemon, once per store path. Without a daemon nothing is valid, as
/// with `--store dummy://`.
pub(crate) struct StoreFs {
    inner: LogicalFs,
    store_dir: StoreDir,
    daemon: Arc<Mutex<crate::store::DaemonState>>,
    validity: Mutex<HashMap<StorePath, bool>>,
}

impl StoreFs {
    pub(crate) fn new(
        store_dir: &StoreDir,
        daemon: Arc<Mutex<crate::store::DaemonState>>,
    ) -> StoreFs {
        StoreFs {
            inner: LogicalFs::new(PathBuf::from(store_dir.as_str()), store_dir.as_str()),
            store_dir: store_dir.clone(),
            daemon,
            validity: Mutex::new(HashMap::new()),
        }
    }

    /// Checks the store path `path` (relative to the store directory)
    /// lies in. The store directory itself has no store path and is left
    /// to the file system. A name that is no store path (`parseStorePath`
    /// fails) is not there.
    fn check(&self, path: &CanonPath) -> std::result::Result<(), AccessorError> {
        let Some(first) = path.components().next() else {
            return Ok(());
        };
        let store_path = match StorePath::new(first) {
            Ok(p) => p,
            Err(_) => {
                return Err(AccessorError::NotFound(prefixed(
                    self.store_dir.as_str().as_bytes(),
                    path,
                )))
            }
        };
        let valid = {
            let mut known = self.validity.lock().expect("validity cache poisoned");
            match known.get(&store_path) {
                Some(v) => *v,
                None => {
                    // The store's answer is remembered; its failure (no
                    // connection) is reported and asked again next time.
                    let v = store_has(&self.daemon, &store_path)
                        .map_err(|e| AccessorError::Io(e.message_bytes().into()))?;
                    known.insert(store_path.clone(), v);
                    v
                }
            }
        };
        if valid {
            Ok(())
        } else {
            Err(AccessorError::Io(
                format!(
                    "path '{}' is not valid",
                    self.store_dir.display(&store_path)
                )
                .into(),
            ))
        }
    }
}

impl SourceAccessor for StoreFs {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        self.check(path)?;
        self.inner.maybe_lstat(path)
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        self.check(path)?;
        self.inner.lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.check(path)?;
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        self.check(path)?;
        self.inner.read_file(path)
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

    fn physical_path(&self, path: &CanonPath) -> Option<std::path::PathBuf> {
        self.inner.physical_path(path)
    }
}

#[cfg(test)]
mod store_fs_tests {
    use super::*;
    use crate::config::Config;
    use bstr::ByteSlice;

    /// The store as pure evaluation reads it: a daemon that cannot be
    /// reached is an I/O error with its message, asked again on every
    /// access; without a store, nothing is valid.
    #[test]
    fn an_unreachable_store_is_an_error_not_an_invalid_path() {
        let store_dir = StoreDir::default();
        let path = CanonPath::new("/00000000000000000000000000000000-brix-unreachable").unwrap();
        let config = Config {
            daemon: Some("unix:///nonexistent/brix-store-fs-test".into()),
            ..Config::default()
        };
        let daemon = crate::store::DaemonState::new(&config, &store_dir).unwrap();
        let fs = StoreFs::new(&store_dir, Arc::new(Mutex::new(daemon)));
        for expected in [
            "cannot connect to socket at '/nonexistent/brix-store-fs-test': No such file or directory",
            "opening a connection to remote store 'unix:///nonexistent/brix-store-fs-test' previously failed",
        ] {
            match fs.maybe_lstat(&path) {
                Err(AccessorError::Io(msg)) => assert!(msg.starts_with_str(expected), "{msg}"),
                other => panic!("expected the connection error, got {other:?}"),
            }
        }
        assert!(fs.validity.lock().unwrap().is_empty(), "nothing remembered");
        let fs = StoreFs::new(
            &store_dir,
            Arc::new(Mutex::new(crate::store::DaemonState::None)),
        );
        match fs.maybe_lstat(&path) {
            Err(AccessorError::Io(msg)) => assert_eq!(
                msg,
                "path '/nix/store/00000000000000000000000000000000-brix-unreachable' is not valid"
            ),
            other => panic!("expected an invalid path, got {other:?}"),
        }
    }

    /// A name under the store directory that is no store path is not
    /// there, shown with its bytes; an allowed directory shows its paths,
    /// whatever their bytes, at their own absolute path.
    #[test]
    fn byte_names_are_shown_as_they_are() {
        let store_dir = StoreDir::default();
        let fs = StoreFs::new(
            &store_dir,
            Arc::new(Mutex::new(crate::store::DaemonState::None)),
        );
        let path = CanonPath::new(b"/caf\xe9/x").unwrap();
        match fs.maybe_lstat(&path) {
            Err(AccessorError::NotFound(shown)) => assert_eq!(shown, b"/nix/store/caf\xe9/x"),
            other => panic!("expected a missing path, got {other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let dir_bytes = brix_store::os_path::path_bytes(dir.path()).unwrap();
        let mut prefix = dir_bytes.to_vec();
        prefix.extend_from_slice(b"/d\xe9");
        std::fs::create_dir(brix_store::os_path::to_path(&prefix).unwrap()).unwrap();
        std::fs::write(
            brix_store::os_path::to_path(&[&prefix[..], b"/caf\xe9"].concat()).unwrap(),
            b"x",
        )
        .unwrap();
        let allowed = CanonPath::new(&prefix).unwrap();
        let root = HostRoot::new(
            &store_dir,
            std::slice::from_ref(&allowed),
            StoreFs::new(
                &store_dir,
                Arc::new(Mutex::new(crate::store::DaemonState::None)),
            ),
        );
        let names: Vec<BString> = root.read_dir(&allowed).unwrap().into_keys().collect();
        assert_eq!(names, [BString::from(b"caf\xe9")]);
        let file = allowed.join_component(b"caf\xe9").unwrap();
        assert_eq!(root.read_file(&file).unwrap(), b"x");
        assert_eq!(
            root.show_path(&file),
            [&prefix[..], b"/caf\xe9"].concat().as_slice()
        );
    }
}
