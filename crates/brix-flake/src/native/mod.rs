//! The fetchers that need the outside world: HTTP, the file system, Git
//! and Mercurial. Everything here is behind the `native` cargo feature.
//!
//! [`NativeFetcher`] dispatches on the input's scheme to the modules of
//! this directory, each of which mirrors one Nix `InputScheme`:
//!
//! | scheme                          | module          | Nix source                 |
//! |---------------------------------|-----------------|----------------------------|
//! | `github`, `gitlab`, `sourcehut` | [`git_archive`] | `libfetchers/github.cc`    |
//! | `tarball`, `file`               | [`tarball`]     | `libfetchers/tarball.cc`   |
//! | `path`                          | [`path`]        | `libfetchers/path.cc`      |
//! | `git`                           | [`git`]         | `libfetchers/git.cc`       |
//! | `hg`                            | [`mercurial`]   | `libfetchers/mercurial.cc` |
//!
//! Around the schemes (the [`Source`] of a fetch) sits the persistent
//! [`cache::FetchCache`], so that an input is downloaded at most once per
//! machine (see [`cache`]); a tree that already exists in a local Nix store
//! is the caller's to find, with [`reuse_from_store`]. A local Git
//! checkout is the exception: like Nix, brix looks at the repository
//! itself on every fetch (see [`NativeFetcher::fetch_workdir`]). The
//! Mercurial fetcher, like Nix's, consults the cache itself.

pub mod cache;
pub mod git;
pub mod git_archive;
pub mod git_attrs;
pub mod git_http;
pub mod git_legacy;
pub mod git_lfs;
pub mod git_submodules;
pub mod git_verify;
pub mod git_workdir;
pub mod http;
pub mod lock;
pub mod mercurial;
pub mod netrc;
pub mod path;
pub mod stderr;
pub mod tarball;
#[cfg(test)]
pub(crate) mod test_https;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brix_store::{FsAccessor, StoreDir};

use crate::attrs::Attr;
use crate::fetch::{compute_store_path, FetchedTree, Fetcher};
use crate::input::{FetchSettings, Input, Scheme};
use crate::progress::{Activity, ActivityKind, NoProgress, Progress};
use crate::{err, Result};

pub use cache::FetchCache;

/// Where fetched Git repositories and the fetch cache live between runs.
/// Nix uses `~/.cache/nix`; brix keeps its own directory so the two never
/// corrupt each other.
pub fn default_cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("brix"));
    }
    match std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join(".cache").join("brix")),
        None => Err(err!(
            "cannot determine the cache directory: neither $XDG_CACHE_HOME nor $HOME is set"
        )),
    }
}

/// `Input::getSourcePath`: where a local input's files are on this
/// machine (a `path:` input's directory, a local Git checkout's working
/// tree); `None` for inputs that are fetched from elsewhere.
pub fn source_path(input: &Input) -> Result<Option<PathBuf>> {
    match input.scheme {
        Some(Scheme::Path) => Ok(Some(PathBuf::from(path::abs_path(input)?))),
        Some(Scheme::Git) => git::source_path(input),
        Some(Scheme::Mercurial) => mercurial::source_path(input),
        _ => Ok(None),
    }
}

/// The fetcher brix's CLI uses.
pub struct NativeFetcher {
    pub http: http::HttpClient,
    /// Git clone cache; `None` disables Git inputs.
    pub cache_dir: Option<PathBuf>,
    /// The persistent fetch cache; `None` fetches everything every time.
    /// (A tree in a local Nix store is the caller's to look for, with
    /// [`reuse_from_store`]: brix's host asks the daemon about it.)
    pub cache: Option<FetchCache>,
    /// A store whose paths need no copy in the fetch cache: a tree that
    /// turns out to exist there after a fetch is only indexed, not stored,
    /// since the next run finds it through the store (brix's host sets
    /// this to the store directory it reads from).
    pub store_with_trees: Option<StoreDir>,
    /// Where warnings go (Nix's `warn`), such as the one about a Git
    /// working tree with uncommitted changes.
    pub warn: Box<dyn Fn(&str) + Send + Sync>,
    /// Where fetches report what they are doing (see [`crate::progress`]);
    /// set it with [`NativeFetcher::with_progress`] so that `http` reports
    /// there too.
    pub progress: Arc<dyn Progress>,
    /// Where a tree that is not cached comes from: [`Schemes`], unless a
    /// test stands in for the outside world.
    pub source: Box<dyn Source>,
}

/// Where [`NativeFetcher`] gets a tree it does not have: the input's own
/// source (`InputScheme::getAccessor`). Everything in front of it, the
/// caches and the bookkeeping of what a fetch found, is the fetcher's.
pub trait Source: Send + Sync {
    fn fetch(
        &self,
        fetcher: &NativeFetcher,
        settings: &FetchSettings,
        input: &Input,
    ) -> Result<FetchedTree>;
}

/// The real [`Source`]: HTTP, the file system and Git, by scheme.
pub struct Schemes;

impl Source for Schemes {
    fn fetch(
        &self,
        fetcher: &NativeFetcher,
        settings: &FetchSettings,
        input: &Input,
    ) -> Result<FetchedTree> {
        fetcher.fetch_from_scheme(settings, input)
    }
}

/// The default warning sink: `warning: …` on stderr.
pub fn warn_to_stderr(msg: &str) {
    eprintln!("warning: {msg}");
}

impl NativeFetcher {
    /// A fetcher with the default cache directory and no access tokens.
    pub fn new() -> Result<NativeFetcher> {
        let cache_dir = default_cache_dir()?;
        Ok(NativeFetcher {
            http: http::HttpClient::new(),
            cache: Some(FetchCache::open(cache_dir.join("fetch"))),
            cache_dir: Some(cache_dir),
            store_with_trees: None,
            warn: Box::new(warn_to_stderr),
            progress: Arc::new(NoProgress),
            source: Box::new(Schemes),
        })
    }

    /// Point every cache at `dir` (`<dir>/fetch` for the fetch cache,
    /// `<dir>/gitv3` for Git clones).
    pub fn with_cache_dir(mut self, dir: PathBuf) -> NativeFetcher {
        self.cache = Some(FetchCache::open(dir.join("fetch")));
        self.cache_dir = Some(dir);
        self
    }

    /// Report fetches and HTTP transfers to `progress`.
    pub fn with_progress(mut self, progress: Arc<dyn Progress>) -> NativeFetcher {
        self.http = self.http.with_progress(progress.clone());
        self.progress = progress;
        self
    }

    /// `InputScheme::getAccessor` for the input's scheme: always fetches.
    pub fn fetch_from_scheme(
        &self,
        settings: &FetchSettings,
        input: &Input,
    ) -> Result<FetchedTree> {
        match input.scheme {
            Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
                git_archive::fetch(&self.http, settings, input)
            }
            Some(Scheme::Tarball) => tarball::fetch_tarball(&self.http, settings, input),
            Some(Scheme::File) => tarball::fetch_file(&self.http, settings, input),
            Some(Scheme::Path) => path::fetch(input),
            Some(Scheme::Git) => {
                let cache_dir = self
                    .cache_dir
                    .as_ref()
                    .ok_or_else(|| err!("Git inputs are disabled (no cache directory)"))?;
                git::fetch_cached(
                    &self.http,
                    cache_dir,
                    self.cache.as_ref(),
                    settings,
                    input,
                    &*self.warn,
                )
            }
            Some(Scheme::Indirect) => Err(err!(
                "indirect input '{}' cannot be fetched directly",
                input
            )),
            Some(Scheme::Mercurial) => mercurial::fetch(self, settings, input),
            None => Err(err!(
                "cannot fetch unsupported input '{}'",
                crate::attrs::attrs_to_json_string(&input.attrs)
            )),
        }
    }

    /// Record what fetching `requested` taught us: the tree under its NAR
    /// hash, the locked attributes under the tree key, and the `rev` a
    /// mutable reference resolved to.
    fn record(
        &self,
        cache: &FetchCache,
        settings: &FetchSettings,
        requested: &Input,
        fetched: &mut FetchedTree,
    ) -> Result<()> {
        if fetched.input.scheme == Some(Scheme::Path) {
            return Ok(());
        }
        // A commit with Git LFS files is read lazily, as Nix reads it (an
        // LFS object is fetched when its file is first read): it is neither
        // hashed nor kept here, and Nix keeps no copy of it either. Its
        // hash is remembered when `fetch_input` computes it
        // (`nar_hash_computed`), and recalled here.
        if git_lfs::smudges_lazily(&fetched.input)? {
            self.recall_nar_hash(cache, settings, requested, fetched)?;
        } else {
            self.record_tree(cache, settings, requested, fetched)?;
        }
        if let (Some(key), Some(rev)) = (cache::ref_key(requested)?, fetched.input.rev()?) {
            // A local checkout's branches are looked up in the repository.
            if !git::is_local_checkout(requested)? {
                let mut value = crate::attrs::Attrs::new();
                value.insert("rev".into(), Attr::String(rev.to_hex()));
                cache.insert(&key, &value)?;
            }
        }
        Ok(())
    }

    /// [`NativeFetcher::record`] for a tree read lazily: the NAR hash an
    /// earlier `fetch_input` computed for the same tree key (see
    /// [`Fetcher::nar_hash_computed`]), as Nix's fetch-to-store cache
    /// answers by fingerprint, so that nothing is read to hash it again.
    fn recall_nar_hash(
        &self,
        cache: &FetchCache,
        settings: &FetchSettings,
        requested: &Input,
        fetched: &mut FetchedTree,
    ) -> Result<()> {
        if fetched.verified || git::bypasses_fetch_cache(settings, requested)? {
            return Ok(());
        }
        let Some((key, _)) = cache::tree_key(&fetched.input)? else {
            return Ok(());
        };
        let known = cache.lookup(&key, None)?;
        if let Some(Attr::String(hash)) = known.and_then(|attrs| attrs.get("narHash").cloned()) {
            fetched
                .input
                .attrs
                .insert("narHash".into(), Attr::String(hash));
            fetched.verified = true;
        }
        Ok(())
    }

    /// [`NativeFetcher::record`] for a tree kept in the cache: under its
    /// NAR hash, and its locked attributes under the tree key.
    fn record_tree(
        &self,
        cache: &FetchCache,
        settings: &FetchSettings,
        requested: &Input,
        fetched: &mut FetchedTree,
    ) -> Result<()> {
        // Only a hash the fetcher vouches for is the tree's: one it merely
        // kept from the input is what the tree should have, which
        // `fetch_input` checks, and a tree filed under it would be served
        // to the next fetch asking for it, unchecked.
        let vouched = match fetched.verified {
            true => fetched.input.nar_hash()?,
            false => None,
        };
        let nar_hash = match vouched {
            Some(h) => h,
            None => {
                let h = crate::fetch::tree_nar_hash(fetched.accessor.as_ref())?;
                fetched
                    .input
                    .attrs
                    .insert("narHash".into(), Attr::String(h.to_sri()));
                fetched.verified = true;
                h
            }
        };
        let in_store = self.store_with_trees.as_ref().is_some_and(|store| {
            compute_store_path(store, &fetched.input)
                .map(|p| Path::new(&store.display(&p)).exists())
                .unwrap_or(false)
        });
        if !in_store {
            cache.store_tree(&nar_hash, fetched.accessor.as_ref())?;
        }
        // A Git commit whose tree is not told by its attributes is never
        // looked up by them (see `resolve_cached`).
        if let (Some((key, _)), false) = (
            cache::tree_key(&fetched.input)?,
            git::bypasses_fetch_cache(settings, requested)?,
        ) {
            cache.insert(&key, &fetched.input.attrs)?;
        }
        Ok(())
    }

    /// A local Git working tree (a `git+file:` input without `ref` or
    /// `rev`), read in place on every fetch as Nix does: its status
    /// decides between a clean checkout of `HEAD` and a dirty tree.
    ///
    /// A clean tree's `revCount` and `lastModified` are remembered by
    /// revision, and its NAR hash, once computed, by its fingerprint
    /// (`git:<rev>`, as Nix's fetch-to-store cache keeps it, mounted
    /// submodules or not). Nothing is remembered about a dirty tree, and
    /// no copy of a working tree is kept: the files are read where they
    /// are.
    fn fetch_workdir(
        &self,
        settings: &FetchSettings,
        input: &Input,
        path: &Path,
    ) -> Result<FetchedTree> {
        let workdir = git_workdir::Workdir::open(path)?;
        // Mounted submodule checkouts are not covered by the revision.
        let key = match (&self.cache, workdir.info.clean_rev()) {
            (Some(_), Some(rev)) if !git::mounts_workdir_submodules(input, &workdir)? => {
                let mut probe = input.clone();
                probe.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
                cache::tree_key(&probe)?.map(|(key, _)| key)
            }
            _ => None,
        };
        let known = match (&self.cache, &key) {
            (Some(cache), Some(key)) => cache.lookup(key, None)?,
            _ => None,
        };
        let cache_dir = self
            .cache_dir
            .as_ref()
            .ok_or_else(|| err!("Git inputs are disabled (no cache directory)"))?;
        let fetched = git::fetch_workdir(
            &self.http,
            cache_dir,
            self.cache.as_ref(),
            settings,
            input,
            &workdir,
            known.as_ref(),
            &*self.warn,
        )?;
        if let (Some(cache), Some(key), None) = (&self.cache, &key, &known) {
            // `ref` is the branch that happens to be checked out, not a
            // property of the revision's tree. The tree's NAR hash is
            // remembered by its fingerprint when it is computed, as Nix's
            // `fetchToStore` does (see `Fetcher::remembered_nar_hash`).
            let mut remembered = fetched.input.attrs.clone();
            remembered.remove("ref");
            remembered.remove("narHash");
            cache.insert(key, &remembered)?;
        }
        Ok(fetched)
    }
}

impl NativeFetcher {
    /// The cached tree of a locked candidate (see
    /// [`Fetcher::locked_candidate`]), if the fetch cache has it.
    fn cached_tree(&self, candidate: &Input) -> Result<Option<FetchedTree>> {
        let (Some(cache), Some(hash)) = (&self.cache, candidate.nar_hash()?) else {
            return Ok(None);
        };
        Ok(cache.tree_dir(&hash)?.map(|root| FetchedTree {
            accessor: Arc::new(FsAccessor::new(root)),
            input: candidate.clone(),
            verified: true,
        }))
    }

    /// For a download (a tarball, a file, a forge's archive) whose
    /// `narHash` names no cached tree: the tree the cache knows for the
    /// rest of the input ([`Fetcher::resolve_cached`] without the hash),
    /// with the attributes it was fetched with, as Nix's fetcher answers
    /// from its own cache whatever hash the input asks for.
    fn downloaded_tree(
        &self,
        settings: &FetchSettings,
        input: &Input,
    ) -> Result<Option<FetchedTree>> {
        let download = matches!(
            input.scheme,
            Some(
                Scheme::Tarball
                    | Scheme::File
                    | Scheme::GitHub
                    | Scheme::GitLab
                    | Scheme::SourceHut
            )
        );
        if !download || input.nar_hash()?.is_none() {
            return Ok(None);
        }
        let mut unhashed = input.clone();
        unhashed.attrs.remove("narHash");
        let Some(candidate) = self.resolve_cached(settings, &unhashed)? else {
            return Ok(None);
        };
        if let Some(tree) = self.cached_tree(&candidate)? {
            return Ok(Some(tree));
        }
        // A tree the store has is only indexed (`record_tree`).
        match &self.store_with_trees {
            Some(store) => reuse_from_store(store, &candidate),
            None => Ok(None),
        }
    }

    /// A remote Git input that verifies its commit, with the `rev` its
    /// `ref` resolved to when the fetch cache has that from less than
    /// `tarball-ttl` ago: the commit is then in the clone, and the fetch
    /// reads it there, as Nix resolves a ref its clone fetched within
    /// `tarball-ttl` without fetching. Only its tree is not taken from the
    /// cache, so that the commit is verified again.
    fn with_cached_rev(&self, settings: &FetchSettings, input: &Input) -> Result<Input> {
        let mut input = input.clone();
        let Some(cache) = &self.cache else {
            return Ok(input);
        };
        // A local checkout's branches are resolved in the repository.
        if git::is_local_checkout(&input)? {
            return Ok(input);
        }
        if let Some(key) = cache::ref_key(&input)? {
            let ttl = Duration::from_secs(settings.tarball_ttl);
            if let Some(rev) = cache
                .lookup(&key, Some(ttl))?
                .and_then(|v| v.get("rev").cloned())
            {
                input.attrs.insert("rev".into(), rev);
            }
        }
        Ok(input)
    }
}

impl Fetcher for NativeFetcher {
    fn fetch(&self, settings: &FetchSettings, input: &Input) -> Result<FetchedTree> {
        // `verifyCommit` runs on every fetch that reaches Nix's Git
        // fetcher: a commit to verify is fetched from its repository every
        // time, never served from the cache (see `git_verify`).
        let verifying = git_verify::wants_verification(input)?;
        let resolved;
        let input = if verifying {
            resolved = self.with_cached_rev(settings, input)?;
            &resolved
        } else {
            input
        };
        if input.scheme != Some(Scheme::Path) && !verifying {
            // `Input::getAccessorUnchecked`: a `narHash` alone is enough to
            // find the tree by content (Nix substitutes the store path and
            // returns the input as given); otherwise the cache must know
            // the rest of the locked attributes.
            if let Some(candidate) = self.locked_candidate(settings, input)? {
                if let Some(mut tree) = self.cached_tree(&candidate)? {
                    // Nix takes a final input with a `narHash` from the
                    // store; anything else it fetches again, and a Git
                    // tree then has the fetcher's fingerprint, under
                    // which its NAR hash may be remembered.
                    let from_store = input.is_final() && input.nar_hash()?.is_some();
                    if candidate.scheme == Some(Scheme::Git) && !from_store {
                        if let Some(fingerprint) = candidate.fingerprint()? {
                            tree.accessor = git::with_fingerprint(
                                tree.accessor,
                                fingerprint,
                                self.cache.as_ref(),
                            );
                        }
                    }
                    return Ok(tree);
                }
            }
            // A `narHash` the cache has no tree for: a download's own
            // cache may still know the tree by the rest of the input (Nix's
            // `tarball` cache entry for a URL, within `tarball-ttl`; the
            // forges' by revision), and Nix's fetcher answers from it rather
            // than downloading (and unpacking) again. `fetch_input` then
            // finds the hash that was asked for wrong.
            if let Some(tree) = self.downloaded_tree(settings, input)? {
                return Ok(tree);
            }
        }
        if let Some(path) = git::local_workdir(input)? {
            if self.cache_dir.is_none() {
                return Err(err!("Git inputs are disabled (no cache directory)"));
            }
            return self.fetch_workdir(settings, input, &path);
        }
        // Reading a local `path:` tree is not worth reporting; everything
        // else is a real fetch (download, unpack, hash, cache).
        let _activity = (input.scheme != Some(Scheme::Path)).then(|| {
            Activity::new(
                &*self.progress,
                ActivityKind::Fetch,
                &format!("fetching '{input}'"),
            )
        });
        let mut fetched = self.source.fetch(self, settings, input)?;
        if let Some(cache) = &self.cache {
            self.record(cache, settings, input, &mut fetched)?;
        }
        Ok(fetched)
    }

    fn has_locally(&self, settings: &FetchSettings, input: &Input) -> Result<bool> {
        if input.scheme == Some(Scheme::Path) || git::local_workdir(input)?.is_some() {
            return Ok(true);
        }
        // A commit to verify is read from its repository on every fetch.
        if git_verify::wants_verification(input)? {
            return git::is_local_checkout(input);
        }
        // What `fetch` checks before fetching.
        match self.locked_candidate(settings, input)? {
            Some(candidate) => Ok(self.cached_tree(&candidate)?.is_some()),
            None => Ok(false),
        }
    }

    /// Remembers the NAR hash of a tree read lazily (a commit with Git LFS
    /// files, see [`NativeFetcher::record`]) under its tree key, as Nix's
    /// fetch-to-store cache keeps it by fingerprint.
    fn nar_hash_computed(
        &self,
        settings: &FetchSettings,
        result: &Input,
        hash: &crate::hashes::NarHash,
    ) -> Result<()> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        if !git_lfs::smudges_lazily(result)? || git::bypasses_fetch_cache(settings, result)? {
            return Ok(());
        }
        if let Some((key, _)) = cache::tree_key(result)? {
            // As `record` files a fetched input: locked, not final.
            let mut attrs = result.attrs.clone();
            attrs.remove("__final");
            attrs.insert("narHash".into(), Attr::String(hash.to_sri()));
            cache.insert(&key, &attrs)?;
        }
        Ok(())
    }

    fn remembered_nar_hash(
        &self,
        _settings: &FetchSettings,
        fingerprint: &str,
    ) -> Result<Option<crate::hashes::NarHash>> {
        match &self.cache {
            Some(cache) => git::remembered_nar_hash(cache, fingerprint),
            None => Ok(None),
        }
    }

    fn remember_nar_hash(&self, fingerprint: &str, hash: &crate::hashes::NarHash) -> Result<()> {
        match &self.cache {
            Some(cache) => git::remember_nar_hash(cache, fingerprint, hash),
            None => Ok(()),
        }
    }

    fn resolve_cached(&self, settings: &FetchSettings, input: &Input) -> Result<Option<Input>> {
        let Some(cache) = &self.cache else {
            return Ok(None);
        };
        // The Mercurial fetcher keeps its own entries, as Nix's does
        // (`mercurial::fetch`), and reads a `rev` Git would refuse.
        if input.scheme == Some(Scheme::Mercurial) {
            return Ok(None);
        }
        // A working tree is looked at on every fetch (`fetch_workdir`), and
        // so is a commit whose tree or fingerprint its attributes do not
        // tell (`nix-219-compat`, mounted submodules), or that is to be
        // verified (`git_verify`): nothing completes such an input but its
        // own `narHash`.
        if git::local_workdir(input)?.is_some()
            || git::bypasses_fetch_cache(settings, input)?
            || git_verify::wants_verification(input)?
        {
            return Ok(None);
        }
        let ttl = Duration::from_secs(settings.tarball_ttl);
        let mut current = input.clone();
        if let Some(rev) = git::resolve_local_ref(input)? {
            // A branch of a local checkout is resolved in the repository.
            current
                .attrs
                .insert("rev".into(), Attr::String(rev.to_hex()));
        } else if let Some(key) = cache::ref_key(&current)? {
            match cache.lookup(&key, Some(ttl))? {
                Some(value) => {
                    if let Some(rev) = value.get("rev") {
                        current.attrs.insert("rev".into(), rev.clone());
                        if matches!(
                            current.scheme,
                            Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut)
                        ) {
                            // The archive fetchers replace `ref` by `rev`.
                            current.attrs.remove("ref");
                        }
                    }
                }
                None => return Ok(None),
            }
        }
        let Some((key, immutable)) = cache::tree_key(&current)? else {
            return Ok(None);
        };
        let Some(locked) = cache.lookup(&key, (!immutable).then_some(ttl))? else {
            return Ok(None);
        };
        // What was asked for must agree with what is remembered, as
        // `checkLocks` would insist after a real fetch.
        for (name, value) in &current.attrs {
            if let Some(other) = locked.get(name) {
                if other != value {
                    return Ok(None);
                }
            }
        }
        let mut attrs = locked;
        for (name, value) in &current.attrs {
            attrs.entry(name.clone()).or_insert_with(|| value.clone());
        }
        if !attrs.contains_key("ref") {
            if let Some(ref_) = git::local_default_ref(input)? {
                attrs.insert("ref".into(), Attr::String(ref_));
            }
        }
        Input::from_attrs(settings, attrs).map(Some)
    }
}

/// `Input::fetchToStore`'s shortcut: a locked input with a `narHash`
/// denotes the store path `makeFixedOutputPath(NixArchive, narHash,
/// name)`; when that path exists in the local store there is nothing to
/// fetch, and the tree is read from there.
///
/// The tree carries no `lastModified` or `rev` of its own: the result's
/// attributes are exactly the input's, which is what Nix's
/// `Input::getAccessorUnchecked` returns when it finds the store path.
pub fn reuse_from_store(store_dir: &StoreDir, input: &Input) -> Result<Option<FetchedTree>> {
    if input.nar_hash()?.is_none() {
        return Ok(None);
    }
    let store_path = compute_store_path(store_dir, input)?;
    let location = store_dir.display(&store_path);
    if std::fs::symlink_metadata(Path::new(&location)).is_err() {
        return Ok(None);
    }
    Ok(Some(FetchedTree {
        accessor: Arc::new(FsAccessor::new(location)),
        input: input.clone(),
        verified: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::{attrs, Attrs};
    use crate::fetch::fetch_input;
    use brix_store::{CanonPath, MemoryAccessor};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tree(text: &str) -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        m.add_file(
            &CanonPath::new("/hello").unwrap(),
            text.as_bytes().to_vec(),
            false,
        )
        .unwrap();
        m
    }

    fn github_attrs() -> Attrs {
        attrs([
            ("type", "github".into()),
            ("owner", "o".into()),
            ("repo", "r".into()),
            ("rev", "a".repeat(40).into()),
        ])
    }

    /// A stand-in for the outside world that counts its calls and
    /// otherwise answers like a `github:` fetch that found
    /// `lastModified = 7`.
    struct Counting {
        calls: Arc<AtomicU32>,
        text: &'static str,
    }

    impl Source for Counting {
        fn fetch(
            &self,
            _fetcher: &NativeFetcher,
            _settings: &FetchSettings,
            input: &Input,
        ) -> Result<FetchedTree> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut locked = input.clone();
            locked.attrs.remove("ref");
            locked
                .attrs
                .insert("rev".into(), Attr::String("a".repeat(40)));
            locked.attrs.insert("lastModified".into(), Attr::Int(7));
            Ok(FetchedTree {
                accessor: Arc::new(tree(self.text)),
                input: locked,
                verified: false,
            })
        }
    }

    /// The real fetcher, with its caches in `dir` and [`Counting`] as the
    /// outside world; and the count of its calls.
    fn counting(dir: &Path, text: &'static str) -> (NativeFetcher, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let mut fetcher = NativeFetcher::new()
            .unwrap()
            .with_cache_dir(dir.to_path_buf());
        fetcher.source = Box::new(Counting {
            calls: calls.clone(),
            text,
        });
        (fetcher, calls)
    }

    fn calls(counter: &AtomicU32) -> u32 {
        counter.load(Ordering::SeqCst)
    }

    #[test]
    fn second_fetch_of_a_rev_comes_from_the_cache_with_the_same_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let settings = FetchSettings::default();
        let (fetcher, count) = counting(dir.path(), "hi\n");
        let input = Input::from_attrs(&settings, github_attrs()).unwrap();

        let first = fetch_input(&fetcher, &settings, &input).unwrap();
        assert_eq!(calls(&count), 1);
        assert_eq!(first.input.last_modified().unwrap(), Some(7));

        let second = fetch_input(&fetcher, &settings, &input).unwrap();
        assert_eq!(calls(&count), 1, "the cache answered");
        assert!(fetcher.has_locally(&settings, &input).unwrap());
        assert_eq!(second.input.attrs, first.input.attrs);
        assert_eq!(
            second
                .accessor
                .read_file(&CanonPath::new("/hello").unwrap())
                .unwrap(),
            b"hi\n"
        );
        // A fresh fetcher sharing the cache directory also hits.
        let (other, other_count) = counting(dir.path(), "different\n");
        let third = fetch_input(&other, &settings, &input).unwrap();
        assert_eq!(calls(&other_count), 0);
        assert_eq!(third.input.attrs, first.input.attrs);
    }

    #[test]
    fn a_ref_is_resolved_from_the_cache_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = FetchSettings::default();
        let (fetcher, count) = counting(dir.path(), "hi\n");
        let mut a = github_attrs();
        a.remove("rev");
        a.insert("ref".into(), Attr::String("main".into()));
        let by_ref = Input::from_attrs(&settings, a).unwrap();
        assert!(!fetcher.has_locally(&settings, &by_ref).unwrap());

        let first = fetch_input(&fetcher, &settings, &by_ref).unwrap();
        assert_eq!(calls(&count), 1);
        let second = fetch_input(&fetcher, &settings, &by_ref).unwrap();
        assert_eq!(calls(&count), 1);
        assert_eq!(second.input.attrs, first.input.attrs);

        // With a zero TTL the ref must be asked again, even within the
        // second it was remembered in.
        settings.tarball_ttl = 0;
        fetch_input(&fetcher, &settings, &by_ref).unwrap();
        assert_eq!(calls(&count), 2);
    }

    /// A download the cache has, asked for with a `narHash` it does not
    /// have: the cached tree is checked against the hash, as Nix's
    /// fetcher answers from its own cache, and nothing is downloaded.
    #[test]
    fn a_wrong_nar_hash_of_a_cached_download_is_caught_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let settings = FetchSettings::default();
        let (fetcher, count) = counting(dir.path(), "x\n");
        let by_rev = Input::from_attrs(&settings, github_attrs()).unwrap();
        fetch_input(&fetcher, &settings, &by_rev).unwrap();
        assert_eq!(calls(&count), 1);
        let mut a = github_attrs();
        a.insert(
            "narHash".into(),
            Attr::String("sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into()),
        );
        let wrong = Input::from_attrs(&settings, a).unwrap();
        let err = fetch_input(&fetcher, &settings, &wrong).unwrap_err();
        assert!(err.message().starts_with("NAR hash mismatch"), "{err}");
        assert_eq!(err.exit_status(), 102);
        assert_eq!(calls(&count), 1);
    }

    /// A lock whose `narHash` does not match the tree it names is caught
    /// when the tree is fetched.
    #[test]
    fn a_wrong_nar_hash_is_caught_after_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let settings = FetchSettings::default();
        let mut a = github_attrs();
        a.insert(
            "narHash".into(),
            Attr::String("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()),
        );
        let mut wrong = Input::from_attrs(&settings, a).unwrap();
        wrong.set_final();
        let (fetcher, count) = counting(dir.path(), "x\n");
        let err = fetch_input(&fetcher, &settings, &wrong).unwrap_err();
        assert!(err.message().starts_with("NAR hash mismatch"), "{err}");
        assert_eq!(err.exit_status(), 102);
        assert_eq!(calls(&count), 1);
        // The tree was filed under its own hash, not the one asked for: a
        // second fetch (answered from the cache by revision) is no more
        // served the wrong tree as the right one than the first.
        let err = fetch_input(&fetcher, &settings, &wrong).unwrap_err();
        assert!(err.message().starts_with("NAR hash mismatch"), "{err}");
        assert_eq!(calls(&count), 1);
        let asked = wrong.nar_hash().unwrap().unwrap();
        assert!(fetcher
            .cache
            .as_ref()
            .unwrap()
            .tree_dir(&asked)
            .unwrap()
            .is_none());
    }

    /// `reuse_from_store`: a locked input with a `narHash` names its store
    /// path; when the path exists the tree is read from there, with the
    /// input's attributes as they are (Nix's `getAccessorUnchecked`).
    #[test]
    fn a_nar_hash_finds_the_tree_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let store_dir = StoreDir::new(store_root.to_str().unwrap());
        let settings = FetchSettings::default();
        let hash = crate::fetch::tree_nar_hash(&tree("x\n")).unwrap();
        let mut a = github_attrs();
        a.insert("narHash".into(), Attr::String(hash.to_sri()));
        let input = Input::from_attrs(&settings, a).unwrap();
        assert!(reuse_from_store(&store_dir, &input).unwrap().is_none());

        let store_path = compute_store_path(&store_dir, &input).unwrap();
        let at = store_dir.display(&store_path);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(Path::new(&at).join("hello"), "x\n").unwrap();
        let reused = reuse_from_store(&store_dir, &input).unwrap().unwrap();
        assert!(reused.verified);
        assert_eq!(reused.input.attrs, input.attrs);
        assert_eq!(reused.input.last_modified().unwrap(), None);
        assert_eq!(reused.store_path(&store_dir).unwrap(), store_path);
        assert_eq!(
            reused
                .accessor
                .read_file(&CanonPath::new("/hello").unwrap())
                .unwrap(),
            b"x\n"
        );
        // Without a `narHash` there is nothing to look for.
        let by_rev = Input::from_attrs(&settings, github_attrs()).unwrap();
        assert!(reuse_from_store(&store_dir, &by_rev).unwrap().is_none());
    }

    fn input_of(url: &str) -> Input {
        let opts = crate::ParseOptions {
            base_dir: Some("/".to_owned()),
            allow_missing: true,
            ..crate::ParseOptions::flake()
        };
        crate::parse_flake_ref(&FetchSettings::default(), url, &opts)
            .unwrap()
            .input
    }

    #[test]
    fn source_paths_of_local_inputs() {
        assert_eq!(
            source_path(&input_of("path:/a/./b/../c")).unwrap(),
            Some(PathBuf::from("/a/c"))
        );
        // A `git+file:` URL to a directory that does not exist is not bare.
        assert_eq!(
            source_path(&input_of("git+file:///no/such/repo?dir=sub")).unwrap(),
            Some(PathBuf::from("/no/such/repo"))
        );
        assert_eq!(
            source_path(&input_of("git+https://example.org/repo")).unwrap(),
            None
        );
        assert_eq!(source_path(&input_of("github:NixOS/nix")).unwrap(), None);
    }

    #[test]
    fn a_bare_repository_has_no_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("git+file://{}", dir.path().display());
        assert_eq!(source_path(&input_of(&url)).unwrap(), None);
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(
            source_path(&input_of(&url)).unwrap(),
            Some(dir.path().to_path_buf())
        );
    }
}
