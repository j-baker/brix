//! The persistent fetch cache (`src/libfetchers/cache.cc` and the store
//! reuse in `Input::fetchToStore`), so that an input fetched once is not
//! downloaded, unpacked and hashed again by the next brix process.
//!
//! Nix keeps two things: a SQLite table mapping a key (the attributes of
//! an input) to the attributes learned by fetching it, and the fetched
//! trees themselves, which live in the Nix store under their NAR hash.
//! brix has no store, so both live under `<cache dir>/fetch`:
//!
//! ```text
//! fetch/index/<nix32 sha256 of the key JSON>.json
//!     { "key": {…}, "value": {…}, "timestamp": <seconds since the epoch> }
//! fetch/trees/<nix32 sha256 NAR hash>/manifest.json
//!     { "narHash": "sha256-…" }
//! fetch/trees/<nix32 sha256 NAR hash>/root[/…]
//!     the tree itself
//! ```
//!
//! Trees are stored unpacked rather than as NAR files because loading is
//! then free: an [`FsAccessor`] reads only the files evaluation touches
//! (a nixpkgs evaluation reads a few hundred of its forty thousand
//! files), whereas a NAR has to be read and parsed in full before the
//! first lookup. Like the Nix store, a tree is trusted once written: its
//! directory name is its NAR hash, it is written to a temporary directory
//! and renamed into place, and an incomplete tree never has a manifest.
//! A cache opened with [`FetchCache::verifying`] rehashes trees on every
//! load (brix's host does so when `BRIX_FETCH_CACHE_VERIFY=1`).
//!
//! Two kinds of index entry mirror Nix's cache keys:
//!
//! * a **tree key** identifies a locked tree ([`tree_key`]): a `rev` for
//!   `github:`-style and `git` inputs, a URL for `tarball:`/`file:`
//!   inputs. Its value is the input's complete locked attributes
//!   (`narHash`, `lastModified`, `revCount`, …). Entries with a `rev` are
//!   immutable; URL entries expire after `tarball-ttl`.
//! * a **ref key** maps a mutable reference ([`ref_key`]: a branch or tag
//!   name) to the `rev` it pointed to, and expires after `tarball-ttl`.
//!
//! Mercurial inputs have neither: their fetcher keeps Nix's two keys for
//! them ([`hg_rev_key`], [`hg_ref_key`]).
//!
//! Every write goes through a temporary file and an atomic rename, so two
//! brix processes may share a cache; the loser of a race merely redoes
//! some work.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brix_store::{CanonPath, FileType, FsAccessor, Hash, HashAlgo, HashFormat, SourceAccessor};

use super::lock::FileLock;
use crate::attrs::{attrs_to_json, json_to_attrs, Attr, Attrs};
use crate::hashes::NarHash;
use crate::input::{Input, Scheme};
use crate::{err, Error, Result};

/// The marker attribute that keeps tree keys and ref keys apart.
const KIND: &str = "_brixCacheKind";

/// A handle on the cache directory. Cheap to clone; opening creates
/// nothing until the first write.
#[derive(Clone, Debug)]
pub struct FetchCache {
    dir: PathBuf,
    /// Rehash a tree whenever it is looked up.
    verify: bool,
}

/// One index entry as stored on disk.
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    key: serde_json::Value,
    value: serde_json::Value,
    timestamp: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    #[serde(rename = "narHash")]
    nar_hash: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io(context: impl Into<String>, e: std::io::Error) -> Error {
    Error::Io {
        context: context.into(),
        source: e,
    }
}

/// A unique name for a temporary file or directory next to its target:
/// unique across processes (pid) and within one (a counter), so threads
/// writing the same tree at the same instant never share a temp name.
fn temp_name(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".tmp-{prefix}-{}-{n}-{nanos}", std::process::id())
}

/// Removes what writers that never finished left in `dir`: a process that
/// exits (or is killed) while writing a tree leaves its temporary
/// directory behind, and brix ends with background fetches of inputs
/// evaluation never needed still running. A writer locks
/// `<temporary directory>.lock` before it creates the directory and until
/// it has renamed it into place, so a temporary directory whose lock can
/// be taken is stale, and so is one without a lock file (an older brix
/// made it). Best effort: what cannot be removed now is left for next
/// time.
fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Lock files go with their directories: one whose directory does
        // not exist (yet) may be a writer's that has not locked it yet.
        if !name.starts_with(".tmp-") || name.ends_with(".lock") {
            continue;
        }
        let temp = dir.join(name);
        let lock_path = with_lock_suffix(&temp);
        let lock = if lock_path.exists() {
            // Not made again if it is gone: its writer removes it once the
            // directory is in place, and one made here would be left
            // behind with nothing to lock.
            match FileLock::try_acquire_existing(&lock_path) {
                Ok(Some(lock)) => Some(lock),
                // Its writer is at work or has just finished, or the lock
                // cannot be looked at.
                Ok(None) | Err(_) => continue,
            }
        } else {
            None
        };
        let removed = std::fs::remove_dir_all(&temp).or_else(|_| std::fs::remove_file(&temp));
        if removed.is_ok() && lock.is_some() {
            // Best effort, like the removal itself.
            let _ = std::fs::remove_file(&lock_path);
        }
    }
}

/// `<path>.lock`.
fn with_lock_suffix(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

impl FetchCache {
    /// The cache at `dir` (`<cache dir>/fetch` for the default fetcher).
    pub fn open(dir: impl Into<PathBuf>) -> FetchCache {
        FetchCache {
            dir: dir.into(),
            verify: false,
        }
    }

    /// The same cache, rehashing every tree it hands out (when `verify`)
    /// to catch trees changed behind its back.
    pub fn verifying(mut self, verify: bool) -> FetchCache {
        self.verify = verify;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn index_file(&self, key: &Attrs) -> PathBuf {
        let json = attrs_to_json(key).to_string();
        let name = Hash::of(HashAlgo::Sha256, json.as_bytes()).to_string(HashFormat::Nix32, false);
        self.dir.join("index").join(format!("{name}.json"))
    }

    /// The value stored for `key`, if there is one and, for a mutable
    /// key, if it has not expired (`Cache::lookupWithTTL`): an entry
    /// stamped at `t` expires once `t + ttl` is in the past, and with a
    /// TTL of 0 (`--refresh`) every entry has expired. `None` for `ttl`
    /// means the entry never expires; so does, in practice, `nix
    /// --offline`'s `u32::MAX` seconds (until 2106 for an entry from 1970).
    pub fn lookup(&self, key: &Attrs, ttl: Option<Duration>) -> Result<Option<Attrs>> {
        let path = self.index_file(key);
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(format!("reading '{}'", path.display()), e)),
        };
        let entry: Entry = serde_json::from_slice(&text).map_err(|e| {
            Error::Json(format!(
                "fetch cache entry '{}' is corrupt: {e}",
                path.display()
            ))
        })?;
        let stored_key = json_to_attrs(&entry.key)?;
        if &stored_key != key {
            // A hash collision or a damaged file: ignore rather than serve
            // the wrong input.
            return Ok(None);
        }
        if let Some(ttl) = ttl {
            // `Cache::lookupExpired`.
            let expired =
                ttl.as_secs() == 0 || entry.timestamp.saturating_add(ttl.as_secs()) < now();
            if expired {
                return Ok(None);
            }
        }
        json_to_attrs(&entry.value).map(Some)
    }

    /// Record `value` for `key`, replacing any older entry.
    pub fn insert(&self, key: &Attrs, value: &Attrs) -> Result<()> {
        let path = self.index_file(key);
        let dir = path.parent().expect("index files live in a directory");
        std::fs::create_dir_all(dir).map_err(|e| io(format!("creating '{}'", dir.display()), e))?;
        let entry = Entry {
            key: attrs_to_json(key),
            value: attrs_to_json(value),
            timestamp: now(),
        };
        let text = serde_json::to_vec_pretty(&entry).expect("entries serialise");
        let tmp = dir.join(temp_name("index"));
        std::fs::write(&tmp, text).map_err(|e| io(format!("writing '{}'", tmp.display()), e))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(
                format!("renaming '{}' to '{}'", tmp.display(), path.display()),
                e,
            )
        })
    }

    fn tree_base(&self, nar_hash: &NarHash) -> PathBuf {
        let name = Hash::from_bytes(HashAlgo::Sha256, nar_hash.as_bytes())
            .to_string(HashFormat::Nix32, false);
        self.dir.join("trees").join(name)
    }

    /// The root of the cached tree with this NAR hash, if the cache holds
    /// a complete copy.
    pub fn tree_dir(&self, nar_hash: &NarHash) -> Result<Option<PathBuf>> {
        let base = self.tree_base(nar_hash);
        let manifest_path = base.join("manifest.json");
        let text = match std::fs::read(&manifest_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(format!("reading '{}'", manifest_path.display()), e)),
        };
        let manifest: Manifest = serde_json::from_slice(&text).map_err(|e| {
            Error::Json(format!(
                "fetch cache manifest '{}' is corrupt: {e}",
                manifest_path.display()
            ))
        })?;
        if manifest.nar_hash != nar_hash.to_sri() {
            return Err(err!(
                "fetch cache tree '{}' claims NAR hash '{}' but is filed under '{}'",
                base.display(),
                manifest.nar_hash,
                nar_hash.to_sri()
            ));
        }
        let root = base.join("root");
        match std::fs::symlink_metadata(&root) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(format!("inspecting '{}'", root.display()), e)),
        }
        if self.verify {
            let actual = crate::fetch::tree_nar_hash(&FsAccessor::new(&root))?;
            if actual != *nar_hash {
                return Err(err!(
                    "fetch cache tree '{}' is corrupt: expected NAR hash '{}' but got '{}'",
                    root.display(),
                    nar_hash.to_sri(),
                    actual.to_sri()
                ));
            }
        }
        Ok(Some(root))
    }

    /// Store `accessor`'s tree under `nar_hash`, which must be its NAR
    /// hash (the caller has just computed it). Returns the root of the
    /// stored copy. A tree already present is left alone.
    pub fn store_tree(&self, nar_hash: &NarHash, accessor: &dyn SourceAccessor) -> Result<PathBuf> {
        if let Some(existing) = self.tree_dir(nar_hash)? {
            return Ok(existing);
        }
        let base = self.tree_base(nar_hash);
        let trees = base.parent().expect("trees live in a directory");
        std::fs::create_dir_all(trees)
            .map_err(|e| io(format!("creating '{}'", trees.display()), e))?;
        remove_stale_temps(trees);
        let tmp = trees.join(temp_name("tree"));
        // Held until the tree is in place: it tells `remove_stale_temps`
        // in other processes that the temporary directory is being written.
        let lock_path = with_lock_suffix(&tmp);
        let lock = FileLock::acquire(&lock_path)?;
        let stored = self.write_tree_at(nar_hash, accessor, &tmp, &base);
        // The lock file goes before the lock: nobody waits for it.
        let _ = std::fs::remove_file(&lock_path);
        drop(lock);
        stored
    }

    /// [`FetchCache::store_tree`]'s writing: the tree and its manifest in
    /// `tmp`, renamed to `base`.
    fn write_tree_at(
        &self,
        nar_hash: &NarHash,
        accessor: &dyn SourceAccessor,
        tmp: &Path,
        base: &Path,
    ) -> Result<PathBuf> {
        std::fs::create_dir(tmp).map_err(|e| io(format!("creating '{}'", tmp.display()), e))?;
        let result = (|| -> Result<()> {
            let root = tmp.join("root");
            write_tree(accessor, &CanonPath::root(), &root)?;
            let manifest = Manifest {
                nar_hash: nar_hash.to_sri(),
            };
            let manifest_path = tmp.join("manifest.json");
            std::fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest).expect("manifests serialise"),
            )
            .map_err(|e| io(format!("writing '{}'", manifest_path.display()), e))
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(tmp);
            return Err(e);
        }
        match std::fs::rename(tmp, base) {
            Ok(()) => {}
            Err(_) if base.join("manifest.json").exists() => {
                // Another process won the race; its copy is as good.
                let _ = std::fs::remove_dir_all(tmp);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(tmp);
                return Err(io(
                    format!("renaming '{}' to '{}'", tmp.display(), base.display()),
                    e,
                ));
            }
        }
        Ok(base.join("root"))
    }
}

/// Write the tree at `path` in `accessor` to `dest`.
fn write_tree(accessor: &dyn SourceAccessor, path: &CanonPath, dest: &Path) -> Result<()> {
    let stat = accessor.lstat(path)?;
    match stat.file_type {
        FileType::Directory => {
            std::fs::create_dir(dest)
                .map_err(|e| io(format!("creating '{}'", dest.display()), e))?;
            for (name, _) in accessor.read_dir(path)? {
                let child = path.join_component(&name).map_err(|e| err!("{}", e))?;
                write_tree(
                    accessor,
                    &child,
                    &dest.join(brix_store::os_path::to_path(&name).ok_or_else(|| {
                        err!("fetch-cache name cannot be represented on this platform")
                    })?),
                )?;
            }
        }
        FileType::Regular => {
            let mut file = std::fs::File::create(dest)
                .map_err(|e| io(format!("creating '{}'", dest.display()), e))?;
            accessor.read_file_to(path, &mut file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if stat.executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| io(format!("setting permissions of '{}'", dest.display()), e))?;
            }
        }
        FileType::Symlink => {
            let target = accessor.read_link(path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                brix_store::os_path::to_path(&target).expect("Unix paths preserve bytes"),
                dest,
            )
            .map_err(|e| io(format!("creating symlink '{}'", dest.display()), e))?;
            #[cfg(not(unix))]
            return Err(err!(
                "cannot store symlink '{}' in the fetch cache on this platform",
                dest.display()
            ));
        }
        other => {
            return Err(err!(
                "cannot store '{}' in the fetch cache: unsupported file type {:?}",
                accessor.show_path(path),
                other
            ))
        }
    }
    Ok(())
}

/// The index key identifying the locked tree an input denotes, or `None`
/// for inputs that are not cached (`path:`, indirect, unknown, or a
/// `github:`-style/`git` input without a `rev`). The second value says
/// whether the entry is immutable. A `rev` that does not parse is an
/// error.
pub fn tree_key(input: &Input) -> Result<Option<(Attrs, bool)>> {
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("tree".into()));
    let copy = |key: &mut Attrs, names: &[&str]| {
        for n in names {
            if let Some(v) = input.attrs.get(*n) {
                key.insert((*n).to_string(), v.clone());
            }
        }
    };
    let Some(scheme) = input.scheme else {
        return Ok(None);
    };
    Ok(match scheme {
        Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
            if input.rev()?.is_none() {
                return Ok(None);
            }
            copy(&mut key, &["type", "host", "owner", "repo", "rev"]);
            Some((key, true))
        }
        Scheme::Git => {
            if input.rev()?.is_none() {
                return Ok(None);
            }
            // A fetch that verifies its commit is never answered from here
            // (`native::git_verify`), so the keys are not part of the tree.
            copy(
                &mut key,
                &[
                    "type",
                    "url",
                    "rev",
                    "submodules",
                    "exportIgnore",
                    "lfs",
                    "shallow",
                ],
            );
            Some((key, true))
        }
        Scheme::Tarball | Scheme::File => {
            copy(&mut key, &["type", "url"]);
            Some((key, false))
        }
        // Read in place, or never fetched.
        Scheme::Path | Scheme::Indirect => None,
        // Its fetcher keeps entries of its own ([`hg_rev_key`]).
        Scheme::Mercurial => None,
    })
}

/// The index key for resolving a mutable reference to a `rev`, or `None`
/// when the input has a `rev` already or no `ref` to resolve. A `rev`
/// that does not parse is an error.
pub fn ref_key(input: &Input) -> Result<Option<Attrs>> {
    if input.rev()?.is_some() {
        return Ok(None);
    }
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("ref".into()));
    let names: &[&str] = match input.scheme {
        Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
            &["type", "host", "owner", "repo", "ref"]
        }
        Some(Scheme::Git) => &["type", "url", "ref", "shallow"],
        _ => return Ok(None),
    };
    for n in names {
        if let Some(v) = input.attrs.get(*n) {
            key.insert((*n).to_string(), v.clone());
        }
    }
    if !key.contains_key("ref") {
        // `HEAD` is what the fetchers resolve when no ref is given.
        key.insert("ref".into(), Attr::String("HEAD".into()));
    }
    Ok(Some(key))
}

/// The index key for the NAR hash of a store path brix was pointed at
/// (`path:/nix/store/…`): immutable, since store contents are
/// content-addressed.
pub fn store_path_key(store_path: &str) -> Attrs {
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("store-path".into()));
    key.insert("path".into(), Attr::String(store_path.to_owned()));
    key
}

/// The index key of what fetching a Mercurial revision found (Nix's
/// `hgRev` key in `MercurialInputScheme::fetchToStore`, which also names
/// the store): its `revCount` and the NAR hash of its tree. Immutable.
/// Mercurial inputs have no [`tree_key`]: their fetcher keeps its own
/// entries, as Nix's does (see `native::mercurial`).
pub fn hg_rev_key(name: &str, rev: &crate::hashes::GitRev) -> Attrs {
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("hgRev".into()));
    key.insert("name".into(), Attr::String(name.to_owned()));
    key.insert("rev".into(), Attr::String(rev.to_hex()));
    key
}

/// The index key of the revision a Mercurial branch or tag of the
/// repository at `url` named when it was last fetched (Nix's
/// `hgRefToRev`); it expires after `tarball-ttl`.
pub fn hg_ref_key(url: &str, ref_: &str) -> Attrs {
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("hgRefToRev".into()));
    key.insert("url".into(), Attr::String(url.to_owned()));
    key.insert("ref".into(), Attr::String(ref_.to_owned()));
    key
}

/// The index key of the NAR hash Nix's `fetchToStore` remembers for a
/// whole tree by the tree's fingerprint (`makeSourcePathToHashCacheKey(
/// fingerprint, NixArchive, /)`, see `native::git::remembered_nar_hash`);
/// it never expires.
pub fn source_path_hash_key(fingerprint: &str) -> Attrs {
    let mut key = Attrs::new();
    key.insert(KIND.into(), Attr::String("sourcePathToHash".into()));
    key.insert("fingerprint".into(), Attr::String(fingerprint.to_owned()));
    key.insert("method".into(), Attr::String("nar".into()));
    key.insert("path".into(), Attr::String("/".into()));
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;
    use crate::input::FetchSettings;
    use brix_store::MemoryAccessor;

    fn cache() -> (tempfile::TempDir, FetchCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = FetchCache::open(dir.path().join("fetch"));
        (dir, cache)
    }

    fn github(rev: bool) -> Input {
        let mut a = attrs([
            ("type", "github".into()),
            ("owner", "o".into()),
            ("repo", "r".into()),
        ]);
        if rev {
            a.insert("rev".into(), Attr::String("a".repeat(40)));
        } else {
            a.insert("ref".into(), Attr::String("main".into()));
        }
        Input::from_attrs(&FetchSettings::default(), a).unwrap()
    }

    #[test]
    fn index_round_trip_and_ttl() {
        let (_dir, cache) = cache();
        let key = ref_key(&github(false)).unwrap().unwrap();
        assert_eq!(cache.lookup(&key, None).unwrap(), None);
        let value = attrs([("rev", Attr::String("b".repeat(40)))]);
        cache.insert(&key, &value).unwrap();
        assert_eq!(cache.lookup(&key, None).unwrap(), Some(value.clone()));
        assert_eq!(
            cache.lookup(&key, Some(Duration::from_secs(3600))).unwrap(),
            Some(value.clone())
        );
        // A TTL of 0 (`--refresh`) expires even an entry made this second,
        // as `lookupExpired` does.
        assert_eq!(cache.lookup(&key, Some(Duration::ZERO)).unwrap(), None);
        // Age the entry past the TTL.
        let path = cache.index_file(&key);
        let mut entry: Entry = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        entry.timestamp -= 7200;
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        assert_eq!(
            cache.lookup(&key, Some(Duration::from_secs(3600))).unwrap(),
            None
        );
        // Immutable lookups ignore age.
        assert_eq!(cache.lookup(&key, None).unwrap(), Some(value.clone()));
        // Replacing an entry refreshes it.
        cache.insert(&key, &value).unwrap();
        assert_eq!(
            cache.lookup(&key, Some(Duration::from_secs(3600))).unwrap(),
            Some(value)
        );
    }

    /// `nix --offline` takes every earlier download to be current: its
    /// `tarball-ttl` of `u32::MAX` seconds keeps even an entry stamped at
    /// the epoch fresh, where the default hour has long expired it.
    #[test]
    fn the_offline_ttl_keeps_old_entries_fresh() {
        let (_dir, cache) = cache();
        let key = ref_key(&github(false)).unwrap().unwrap();
        let value = attrs([("rev", Attr::String("c".repeat(40)))]);
        cache.insert(&key, &value).unwrap();
        let path = cache.index_file(&key);
        let mut entry: Entry = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        entry.timestamp = 0;
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        let offline = Duration::from_secs(u32::MAX.into());
        assert_eq!(cache.lookup(&key, Some(offline)).unwrap(), Some(value));
        assert_eq!(
            cache.lookup(&key, Some(Duration::from_secs(3600))).unwrap(),
            None
        );
    }

    #[test]
    fn keys_distinguish_kinds_and_schemes() {
        let (tree, immutable) = tree_key(&github(true)).unwrap().unwrap();
        assert!(immutable);
        assert_eq!(tree.get(KIND), Some(&Attr::String("tree".into())));
        assert!(tree.contains_key("rev") && !tree.contains_key("narHash"));
        assert!(
            tree_key(&github(false)).unwrap().is_none(),
            "no rev, no tree key"
        );
        assert!(
            ref_key(&github(true)).unwrap().is_none(),
            "a rev needs no resolving"
        );
        let r = ref_key(&github(false)).unwrap().unwrap();
        assert_eq!(r.get("ref"), Some(&Attr::String("main".into())));
        let tarball = Input::from_url_str(
            &FetchSettings::default(),
            "tarball+https://example.org/x.tar.gz",
            true,
        )
        .unwrap();
        let (_, immutable) = tree_key(&tarball).unwrap().unwrap();
        assert!(!immutable, "a URL's content can change");
        let path =
            Input::from_url_str(&FetchSettings::default(), "path:/some/where", true).unwrap();
        assert!(tree_key(&path).unwrap().is_none());
        // A `rev` that does not parse is an error, not "no rev".
        let mut bad = github(true);
        bad.attrs.insert("rev".into(), Attr::String("nope".into()));
        assert!(tree_key(&bad).is_err());
        assert!(ref_key(&bad).is_err());
        assert_ne!(
            FetchCache::open("/c").index_file(&tree),
            FetchCache::open("/c").index_file(&r)
        );
    }

    #[test]
    fn corrupt_or_colliding_entries_are_ignored() {
        let (_dir, cache) = cache();
        let key = ref_key(&github(false)).unwrap().unwrap();
        let other = tree_key(&github(true)).unwrap().unwrap().0;
        cache.insert(&other, &attrs([("x", Attr::Int(1))])).unwrap();
        // Plant the other entry under this key's file name.
        std::fs::create_dir_all(cache.index_file(&key).parent().unwrap()).unwrap();
        std::fs::copy(cache.index_file(&other), cache.index_file(&key)).unwrap();
        assert_eq!(cache.lookup(&key, None).unwrap(), None);
        std::fs::write(cache.index_file(&key), b"not json").unwrap();
        assert!(cache.lookup(&key, None).is_err());
    }

    fn sample_tree() -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        m.add_file(
            &CanonPath::new("/a/b.txt").unwrap(),
            b"hello".to_vec(),
            false,
        )
        .unwrap();
        m.add_file(
            &CanonPath::new("/a/run.sh").unwrap(),
            b"#!/bin/sh\n".to_vec(),
            true,
        )
        .unwrap();
        m.add_symlink(&CanonPath::new("/link").unwrap(), "a/b.txt")
            .unwrap();
        m.add_dir(&CanonPath::new("/empty").unwrap()).unwrap();
        m
    }

    #[test]
    fn trees_round_trip_and_incomplete_trees_are_ignored() {
        let (_dir, cache) = cache();
        let tree = sample_tree();
        let hash = crate::fetch::tree_nar_hash(&tree).unwrap();
        assert_eq!(cache.tree_dir(&hash).unwrap(), None);
        let root = cache.store_tree(&hash, &tree).unwrap();
        assert_eq!(cache.tree_dir(&hash).unwrap(), Some(root.clone()));
        // The copy hashes to the same NAR hash: contents, executable bits,
        // symlinks and empty directories survived.
        let copied = FsAccessor::new(&root);
        assert_eq!(crate::fetch::tree_nar_hash(&copied).unwrap(), hash);
        // Storing again is a no-op.
        assert_eq!(cache.store_tree(&hash, &tree).unwrap(), root);
        // An interrupted write can also leave a manifest without a root.
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(cache.tree_dir(&hash).unwrap(), None);
        // Without its manifest a tree does not exist as far as the cache
        // is concerned (an interrupted write never produces one).
        std::fs::remove_file(root.parent().unwrap().join("manifest.json")).unwrap();
        assert_eq!(cache.tree_dir(&hash).unwrap(), None);
        // A manifest filed under the wrong hash is corruption, not a miss.
        let wrong = NarHash::from_bytes([7; 32]);
        std::fs::create_dir_all(cache.tree_base(&wrong)).unwrap();
        std::fs::write(
            cache.tree_base(&wrong).join("manifest.json"),
            serde_json::to_vec(&Manifest {
                nar_hash: hash.to_sri(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(cache.tree_dir(&wrong).is_err());
    }

    /// A tree half-written by a process that is gone (its lock can be
    /// taken) or by an older brix (it has no lock) is removed by the next
    /// write; one another process is writing (it holds the lock) is left
    /// alone, whatever its name says.
    #[cfg(unix)]
    #[test]
    fn temporary_trees_of_dead_writers_are_removed() {
        let (dir, cache) = cache();
        let trees = cache.dir().join("trees");
        std::fs::create_dir_all(&trees).unwrap();
        let dead = trees.join(".tmp-tree-1-0-0");
        let legacy = trees.join(".tmp-tree-2-0-0");
        let live = trees.join(".tmp-tree-3-0-0");
        for temp in [&dead, &legacy, &live] {
            std::fs::create_dir_all(temp.join("root")).unwrap();
            std::fs::write(temp.join("root/partial"), b"x").unwrap();
        }
        std::fs::write(with_lock_suffix(&dead), "").unwrap();
        let (ready, release) = (dir.path().join("ready"), dir.path().join("release"));
        let mut writer =
            super::super::lock::tests::spawn_holder(&with_lock_suffix(&live), &ready, &release);
        let tree = sample_tree();
        let hash = crate::fetch::tree_nar_hash(&tree).unwrap();
        cache.store_tree(&hash, &tree).unwrap();
        assert!(!dead.exists() && !with_lock_suffix(&dead).exists());
        assert!(!legacy.exists());
        assert!(live.exists());
        std::fs::write(&release, "").unwrap();
        assert!(writer.wait().unwrap().success());
        // Once its writer is gone, the next write removes it.
        let mut other = MemoryAccessor::new();
        other
            .add_file(&CanonPath::new("/x").unwrap(), b"other".to_vec(), false)
            .unwrap();
        cache
            .store_tree(&crate::fetch::tree_nar_hash(&other).unwrap(), &other)
            .unwrap();
        assert!(!live.exists() && !with_lock_suffix(&live).exists());
    }

    #[test]
    fn verification_detects_tampering() {
        let (_dir, cache) = cache();
        let tree = sample_tree();
        let hash = crate::fetch::tree_nar_hash(&tree).unwrap();
        let root = cache.store_tree(&hash, &tree).unwrap();
        std::fs::write(root.join("a/b.txt"), b"tampered").unwrap();
        // Without verification the tree is trusted, like a store path.
        assert!(cache.tree_dir(&hash).unwrap().is_some());
        let verifying = FetchCache::open(cache.dir()).verifying(true);
        assert!(verifying.tree_dir(&hash).is_err());
    }

    #[test]
    fn concurrent_writers_agree() {
        let (_dir, cache) = cache();
        let tree = sample_tree();
        let hash = crate::fetch::tree_nar_hash(&tree).unwrap();
        let key = tree_key(&github(true)).unwrap().unwrap().0;
        std::thread::scope(|s| {
            for i in 0..8u64 {
                let cache = cache.clone();
                let tree = tree.clone();
                let key = key.clone();
                s.spawn(move || {
                    cache.store_tree(&hash, &tree).unwrap();
                    cache.insert(&key, &attrs([("n", Attr::Int(i))])).unwrap();
                });
            }
        });
        let root = cache.tree_dir(&hash).unwrap().unwrap();
        assert_eq!(
            crate::fetch::tree_nar_hash(&FsAccessor::new(&root)).unwrap(),
            hash
        );
        // Exactly one entry survived, and nothing temporary is left over.
        assert!(cache.lookup(&key, None).unwrap().is_some());
        for sub in ["index", "trees"] {
            let leftovers: Vec<_> = std::fs::read_dir(cache.dir().join(sub))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.starts_with(".tmp-"))
                .collect();
            assert!(leftovers.is_empty(), "{sub}: {leftovers:?}");
        }
    }
}
