//! The host brix's evaluator runs on: the root file system
//! ([`crate::root_fs`]: the store directory and the allowed paths),
//! brix-flake's fetchers, and a Nix daemon for import-from-derivation
//! ([`crate::store::daemon_build`]).
//!
//! This is the one place where evaluation touches the outside world. The
//! evaluator asks through [`brix_eval::Host`]; the flake layer
//! ([`crate::flake`]) asks through the inherent methods.

#[cfg(brix_parallel)]
use brix_eval::parallel_support::cell::{Cell, RefCell};
#[cfg(not(brix_parallel))]
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use brix_eval::{
    BuildRequest, EvalStoreView, FetchAttr, FetchError, FetchRequest, FetchedFile, FetchedTree,
    Host,
};
use brix_flake::{Attr, Attrs, FetchSettings, Input, Progress, Registry, RegistryType, Waiting};
use brix_store::{CanonPath, Hash, MemoryAccessor, SourceAccessor, StoreDir, StorePath};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fetching::{Fetching, Prefetcher, SharedFetcher};
use crate::root_fs::{HostRoot, StoreFs};

/// A tree the host fetched, by the store path it is mounted at.
#[derive(Clone)]
pub(crate) struct Tree {
    pub accessor: Arc<dyn SourceAccessor>,
    /// The locked, final input describing the tree.
    pub input: Input,
    pub store_path: StorePath,
    /// The NAR hash when it is known but, as for a store path the daemon
    /// vouches for, deliberately not part of the input's attributes.
    pub nar_hash: Option<Hash>,
    /// Taken from the store rather than fetched: a final input the store
    /// already had (`makeStoreAccessor`), shown as `«locked url»` whatever
    /// its kind, a local `path:` input included.
    pub from_store: bool,
}

/// A tree mounted lazily (see `brix_eval::lazy_tree`): fetched without its
/// NAR hash, readable at a placeholder, and hashed by the evaluator only
/// if its store path is needed.
#[derive(Clone)]
pub(crate) struct LazyTree {
    pub accessor: Arc<dyn SourceAccessor>,
    /// The locked input, without `narHash`.
    pub input: Input,
    pub id: brix_eval::LazyTreeId,
    /// Where the tree is mounted (`<store>/.brix-lazy-<n>-<name>`).
    pub placeholder: String,
}

/// What [`NativeHost::fetch_for_mount`] found.
pub(crate) enum Fetched {
    /// A tree with a known store path, now in the fetch cache (so
    /// [`NativeHost::fetch_locked`] returns it without fetching again).
    Known,
    /// A tree mounted lazily before.
    Lazy(LazyTree),
    /// A fetched tree nobody has hashed, for the caller to mount lazily.
    Unhashed(brix_flake::FetchedTree),
}

/// How a fetched tree's files are shown: at the checkout's directory for a
/// local `path`/`git` input, else as `«locked url»`.
pub(crate) fn display_prefix(input: &Input) -> Result<String> {
    match local_display_prefix(input) {
        Some(p) => Ok(p),
        None => Ok(format!("«{}»", input.to_url_text(true)?)),
    }
}

/// One entry of `callFlake`'s `overrides`: a node whose tree is already
/// known, so `call-flake.nix` must not fetch it.
#[derive(Clone)]
pub(crate) struct Override {
    pub root: crate::flake::TreeRoot,
    pub subdir: String,
    pub input: Input,
    pub force_dirty: bool,
}

/// The native host. Cache storage is selected with the evaluator backend;
/// fetching also has its own concurrency (see [`crate::fetching`]).
pub struct NativeHost {
    store_dir: StoreDir,
    root: Arc<HostRoot>,
    allowed: Vec<CanonPath>,
    pub(crate) settings: FetchSettings,
    /// Fetching locked trees, shared with the background fetches.
    fetching: Arc<Fetching>,
    prefetcher: Prefetcher,
    /// Where fetches report progress, and where the host says when the
    /// evaluator waits for them.
    progress: Arc<dyn Progress>,
    /// For each `callFlake`, the inputs to fetch in the background once
    /// evaluation needs a node (`brix_flake::prefetch_plan`), by node key.
    prefetch_plans: RefCell<HashMap<u64, HashMap<String, Vec<Input>>>>,
    /// The persistent fetch cache the fetcher writes to, for the NAR hashes
    /// of store paths brix is pointed at directly.
    persistent_cache: Option<brix_flake::FetchCache>,
    http: Option<brix_flake::native::http::HttpClient>,
    /// Fetch results by the attributes they were requested with, so that
    /// locking and `fetchTree` never fetch the same input twice.
    fetch_cache: RefCell<HashMap<Attrs, Tree>>,
    /// Every tree mounted under the store directory, by store path.
    trees: RefCell<HashMap<StorePath, Tree>>,
    /// Trees mounted lazily, by the attributes they were requested with
    /// and by their own (see [`LazyTree`]).
    lazy_cache: RefCell<HashMap<Attrs, LazyTree>>,
    lazy_trees: RefCell<HashMap<brix_eval::LazyTreeId, LazyTree>>,
    pub(crate) daemon: Arc<Mutex<crate::store::DaemonState>>,
    overrides: RefCell<HashMap<u64, HashMap<String, Override>>>,
    next_call: Cell<u64>,
    /// `flake-registry`.
    flake_registry: String,
    registries: RefCell<Option<Vec<Registry>>>,
    /// Compiled-input images (see [`crate::compiled`]).
    images: crate::compiled::ImageStore,
}

impl NativeHost {
    pub(crate) fn new(config: &Config) -> Result<NativeHost> {
        let store_dir = config.store_dir().clone();
        let mut allowed = Vec::new();
        for p in &config.allowed_paths {
            // Bytes, as Nix's paths are; only a system whose paths are not
            // bytes (Windows) has paths that cannot be named this way.
            let bytes = brix_store::os_path::path_bytes(p).ok_or_else(|| {
                Error::Config(format!("allowed path '{}' is not UTF-8", p.display()))
            })?;
            allowed.push(CanonPath::new(bytes)?);
        }
        let settings = FetchSettings {
            access_tokens: config.fetch.access_tokens.clone(),
            experimental_features: config.eval.experimental_features.clone(),
            tarball_ttl: config.fetch.tarball_ttl,
            netrc_file: config.fetch.netrc_file.clone(),
            allow_dirty: config.fetch.allow_dirty,
            warn_dirty: config.fetch.warn_dirty,
            allow_dirty_locks: config.fetch.allow_dirty_locks,
            trust_tarballs_from_git_forges: config.fetch.trust_tarballs_from_git_forges,
            nix219_compat: config.fetch.nix219_compat,
            download_attempts: config.fetch.download_attempts,
            download_retry_delay_ms: config.fetch.download_retry_delay_ms,
            connect_timeout: config.fetch.connect_timeout,
            // Nix's fetchers and URL parsing warn with `warn()`
            // (`fixGitURL`'s SCP warning spans lines): see `log_warning`.
            warn: brix_flake::Warn::new(log_warning),
            notice: if config.fetch.notices {
                brix_flake::Warn::new(crate::log::stderr)
            } else {
                brix_flake::Warn::new(|_| {})
            },
            info: if config.fetch.infos {
                brix_flake::Warn::new(crate::log::stderr)
            } else {
                brix_flake::Warn::new(|_| {})
            },
            // `logError`, shown at every verbosity.
            error: brix_flake::Warn::new(|msg| crate::log::stderr(&format!("error: {msg}"))),
        };
        let fetcher: Option<SharedFetcher> = if config.fetch.enabled {
            Some(Arc::new(native_fetcher(config)?))
        } else {
            None
        };
        let persistent_cache = if config.fetch.enabled {
            Some(
                brix_flake::FetchCache::open(fetch_cache_dir(config)?.join("fetch"))
                    .verifying(config.fetch.verify_cache),
            )
        } else {
            None
        };
        let daemon = Arc::new(Mutex::new(crate::store::DaemonState::new(
            config, &store_dir,
        )?));
        let fetching = Arc::new(Fetching {
            store_dir: store_dir.clone(),
            settings: settings.clone(),
            fetcher,
            daemon: daemon.clone(),
        });
        Ok(NativeHost {
            root: Arc::new(HostRoot::new(
                &store_dir,
                &allowed,
                StoreFs::new(&store_dir, daemon.clone()),
            )),
            store_dir,
            allowed,
            settings,
            prefetcher: Prefetcher::new(fetching.clone(), config.fetch.http_connections),
            fetching,
            progress: config.progress.clone(),
            prefetch_plans: RefCell::new(HashMap::new()),
            persistent_cache,
            http: config.fetch.enabled.then(|| http_client(config)),
            fetch_cache: RefCell::new(HashMap::new()),
            trees: RefCell::new(HashMap::new()),
            lazy_cache: RefCell::new(HashMap::new()),
            lazy_trees: RefCell::new(HashMap::new()),
            daemon,
            overrides: RefCell::new(HashMap::new()),
            next_call: Cell::new(1),
            flake_registry: config.fetch.flake_registry.clone(),
            registries: RefCell::new(None),
            images: crate::compiled::ImageStore::new(
                &config.compiled,
                &fetch_cache_dir(config)?,
                config.progress.clone(),
                brix_store::XpSettings::from_names(
                    config.eval.experimental_features.iter().map(String::as_str),
                ),
                config.eval.timings,
            ),
        })
    }

    pub(crate) fn store_dir(&self) -> &StoreDir {
        &self.store_dir
    }

    pub(crate) fn images(&self) -> &crate::compiled::ImageStore {
        &self.images
    }

    /// An evaluation error as the facade reports it. Every evaluation
    /// error leaving the evaluator comes through here, so that a damaged
    /// compiled image it reveals is dealt with
    /// ([`crate::compiled::ImageStore::forget_damaged`]).
    pub(crate) fn eval_error(&self, e: &brix_eval::Error, show_trace: bool) -> Error {
        self.images.forget_damaged();
        Error::eval(e, show_trace)
    }

    /// Abandons the background fetches no thread has started (see
    /// [`Prefetcher::cancel_queued`]).
    pub(crate) fn cancel_prefetches(&self) {
        self.prefetcher.cancel_queued();
    }

    /// Closes the daemon connection, if one was made (see
    /// [`crate::store::DaemonState::close`]). The session calls this when
    /// it ends.
    pub(crate) fn close_store(&self) {
        self.daemon
            .lock()
            // A panic while the state was locked leaves nothing to keep:
            // close what is there.
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close();
    }

    /// Fetches `input` (or returns the cached result), computes its store
    /// path and remembers the tree. Does not mount it: the evaluator does
    /// that through [`brix_eval::Evaluator::mount_fetched_tree`].
    ///
    /// With `require_lockable` (Nix's `LockFlags::requireLockable`, true
    /// for everything but `nix flake metadata`'s root) the returned
    /// input carries a `narHash`, so it can go into a lock file.
    pub(crate) fn fetch_locked(&self, input: &Input, require_lockable: bool) -> Result<Tree> {
        if let Some(tree) = self.fetch_cache.borrow().get(&input.attrs) {
            let has_hash = tree.input.nar_hash().ok().flatten().is_some();
            if !require_lockable || has_hash {
                return Ok(tree.clone());
            }
        }
        if let Some(tree) = self.store_path_tree(input, require_lockable)? {
            self.remember(input, tree.clone());
            return Ok(tree);
        }
        let _waiting = Waiting::new(&*self.progress);
        // A background fetch of the same input did exactly what fetching
        // it here would do.
        let tree = self.prefetcher.fetch(input)?;
        self.remember(input, tree.clone());
        Ok(tree)
    }

    /// Starts fetching `inputs` in the background (see
    /// [`crate::fetching`]); [`NativeHost::fetch_locked`] picks the
    /// results up. Inputs this session already has are skipped.
    pub(crate) fn prefetch(&self, inputs: impl IntoIterator<Item = Input>) {
        for input in inputs {
            if !self.fetch_cache.borrow().contains_key(&input.attrs) {
                self.prefetcher.start(input);
            }
        }
    }

    /// Registers what to fetch in the background as evaluation of the
    /// `callFlake` with id `call` needs each node (by node key).
    pub(crate) fn register_prefetch_plan(&self, call: u64, plan: HashMap<String, Vec<Input>>) {
        self.prefetch_plans.borrow_mut().insert(call, plan);
    }

    /// Evaluation of `callFlake` `call` needs the tree of the node `key`:
    /// start the fetches its plan lists, once.
    pub(crate) fn node_needed(&self, call: u64, key: &str) {
        let inputs = self
            .prefetch_plans
            .borrow_mut()
            .get_mut(&call)
            .and_then(|plan| plan.remove(key));
        if let Some(inputs) = inputs {
            self.prefetch(inputs);
        }
    }

    /// [`NativeHost::fetch_locked`] for a tree that need not be lockable
    /// (the top-level flake of most commands). A tree whose NAR hash is
    /// not known yet comes back unhashed, for the caller to mount lazily
    /// and hand to [`NativeHost::remember_lazy`]; a tree with a known hash
    /// (cached, in the store, verified by the fetcher) is an ordinary one.
    pub(crate) fn fetch_for_mount(&self, input: &Input) -> Result<Fetched> {
        if let Some(lazy) = self.lazy_cache.borrow().get(&input.attrs) {
            return Ok(Fetched::Lazy(lazy.clone()));
        }
        if self.fetch_cache.borrow().contains_key(&input.attrs) {
            return Ok(Fetched::Known);
        }
        if let Some(tree) = self.store_path_tree(input, false)? {
            self.remember(input, tree);
            return Ok(Fetched::Known);
        }
        if let Some(tree) = self.fetching.local_store_tree(input)? {
            self.remember(input, tree);
            return Ok(Fetched::Known);
        }
        let fetcher = self.fetching.fetcher(input)?;
        let _waiting = Waiting::new(&*self.progress);
        let fetched = brix_flake::fetch_input_unhashed(fetcher, &self.settings, input)?;
        if fetched.input.nar_hash()?.is_none() {
            return Ok(Fetched::Unhashed(fetched));
        }
        let store_path = fetched.store_path(&self.store_dir)?;
        let tree = Tree {
            accessor: fetched.accessor,
            input: fetched.input,
            store_path,
            nar_hash: None,
            from_store: false,
        };
        self.remember(input, tree);
        Ok(Fetched::Known)
    }

    /// Records a tree the caller mounted lazily for `requested`.
    pub(crate) fn remember_lazy(&self, requested: &Input, tree: LazyTree) {
        let mut cache = self.lazy_cache.borrow_mut();
        cache.insert(requested.attrs.clone(), tree.clone());
        cache
            .entry(tree.input.attrs.clone())
            .or_insert_with(|| tree.clone());
        self.lazy_trees.borrow_mut().insert(tree.id, tree);
    }

    /// A lazily mounted tree.
    pub(crate) fn lazy_tree(&self, id: brix_eval::LazyTreeId) -> Option<LazyTree> {
        self.lazy_trees.borrow().get(&id).cloned()
    }

    fn remember(&self, requested: &Input, tree: Tree) {
        let mut cache = self.fetch_cache.borrow_mut();
        cache.insert(requested.attrs.clone(), tree.clone());
        cache
            .entry(tree.input.attrs.clone())
            .or_insert_with(|| tree.clone());
        self.trees
            .borrow_mut()
            .insert(tree.store_path.clone(), tree);
    }

    /// A `path` input naming a store path is its own tree: Nix neither
    /// copies nor re-hashes it (the store knows its NAR hash). The hash
    /// comes from the daemon when there is one and is computed otherwise;
    /// as in Nix it is only recorded in the input when a lockable result
    /// is required.
    fn store_path_tree(&self, input: &Input, require_lockable: bool) -> Result<Option<Tree>> {
        if input.type_name().ok() != Some("path") {
            return Ok(None);
        }
        let Some(Attr::String(path)) = input.attrs.get("path") else {
            return Ok(None);
        };
        let Some(store_path) = self.store_dir.maybe_parse(path) else {
            return Ok(None);
        };
        if self.store_dir.display(&store_path) != *path {
            return Ok(None);
        }
        if !std::path::Path::new(path).is_dir() {
            return Err(Error::Flake(brix_flake::Error::Message(format!(
                "path '{path}' does not exist"
            ))));
        }
        let accessor: Arc<dyn SourceAccessor> = Arc::new(brix_store::FsAccessor::new(path));
        let mut locked = input.clone();
        locked.set_final();
        // `Input::getAccessorUnchecked` first asks the fetcher for what it
        // has at hand and ignores whatever goes wrong (`catch (...)`), then
        // fetches for real: a store that cannot be reached fails silently
        // the first time, and the second time it is the error that it
        // failed before, under the input's trace.
        let mut nar_hash = match self.daemon_nar_hash(&store_path) {
            Ok(hash) => hash,
            Err(_) => self.daemon_nar_hash(&store_path).map_err(|e| {
                Error::Flake(
                    brix_flake::Error::Message(e.to_string())
                        .trace(format!("while fetching the input '{input}'")),
                )
            })?,
        };
        if require_lockable && locked.nar_hash()?.is_none() {
            let hash = match &nar_hash {
                Some(h) => brix_flake::NarHash::from_bytes(
                    h.as_bytes()
                        .try_into()
                        .map_err(|_| Error::Store("NAR hash is not SHA-256".into()))?,
                ),
                None => match self.cached_store_path_hash(path)? {
                    Some(h) => h,
                    None => {
                        let h = brix_flake::fetch::tree_nar_hash(&*accessor)?;
                        self.remember_store_path_hash(path, &h)?;
                        h
                    }
                },
            };
            if nar_hash.is_none() {
                nar_hash = Some(Hash::from_bytes(
                    brix_store::HashAlgo::Sha256,
                    hash.as_bytes(),
                ));
            }
            locked
                .attrs
                .insert("narHash".into(), Attr::String(hash.to_sri()));
        }
        Ok(Some(Tree {
            accessor,
            input: locked,
            nar_hash,
            store_path,
            from_store: false,
        }))
    }

    /// The NAR hash the persistent cache remembers for a store path brix
    /// was pointed at (`path:/nix/store/…`), so it is computed once per
    /// machine rather than once per run.
    fn cached_store_path_hash(&self, path: &str) -> Result<Option<brix_flake::NarHash>> {
        let Some(cache) = &self.persistent_cache else {
            return Ok(None);
        };
        let key = brix_flake::native::cache::store_path_key(path);
        match cache.lookup(&key, None)? {
            Some(attrs) => match attrs.get("narHash") {
                Some(Attr::String(h)) => {
                    Ok(Some(brix_flake::NarHash::parse_sri(h, self.settings.xp())?))
                }
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn remember_store_path_hash(&self, path: &str, hash: &brix_flake::NarHash) -> Result<()> {
        if let Some(cache) = &self.persistent_cache {
            let key = brix_flake::native::cache::store_path_key(path);
            let mut value = Attrs::new();
            value.insert("narHash".into(), Attr::String(hash.to_sri()));
            cache.insert(&key, &value)?;
        }
        Ok(())
    }

    /// The NAR hash of a store path if the daemon knows it; `None` when
    /// it does not, or when there is no store.
    pub(crate) fn daemon_nar_hash(&self, path: &StorePath) -> Result<Option<Hash>> {
        let mut daemon = self.daemon.lock().expect("daemon state poisoned");
        let Some(client) = daemon.try_client()? else {
            return Ok(None);
        };
        Ok(client.query_path_info(path)?.map(|info| info.nar_hash))
    }

    /// The tree mounted at `store_path`, if this host fetched it.
    pub(crate) fn tree(&self, store_path: &StorePath) -> Option<Tree> {
        self.trees.borrow().get(store_path).cloned()
    }

    /// A fetched tree for the evaluator to mount, with the attributes
    /// `fetchTree` returns for it (`emitTreeAttrs`, through
    /// [`brix_flake::source_info`]). `empty_rev_fallback` is the
    /// request's (`fetchGit`).
    pub(crate) fn to_eval_tree(
        &self,
        tree: &Tree,
        empty_rev_fallback: bool,
    ) -> Result<FetchedTree> {
        let out_path = self.store_dir.display(&tree.store_path);
        let mut info = brix_flake::source_info(&out_path, &tree.input, empty_rev_fallback, false)?;
        info.remove("outPath");
        // An input without a `narHash` has Nix compute it on demand; the
        // fetch already did.
        if let Some(h) = &tree.nar_hash {
            info.entry("narHash".to_owned())
                .or_insert_with(|| Attr::String(h.to_string(brix_store::HashFormat::Sri, true)));
        }
        let attrs = attrs_to_fetch(&info);
        Ok(FetchedTree {
            accessor: tree.accessor.clone(),
            store_path: tree.store_path.clone(),
            attrs,
            locked_attrs: attrs_to_fetch(&tree.input.attrs),
            locked_url: tree.input.to_url_text(true)?,
            // Text (a `path` or `file://` URL attribute), whatever type
            // the evaluator keeps it as.
            display_prefix: if tree.from_store {
                None
            } else {
                local_display_prefix(&tree.input)
            },
        })
    }

    /// Registers the `overrides` of one `callFlake` and returns the call
    /// id the Nix side passes back to look them up.
    pub(crate) fn register_overrides(&self, overrides: HashMap<String, Override>) -> u64 {
        #[cfg(not(brix_parallel))]
        let id = {
            let id = self.next_call.get();
            self.next_call.set(id + 1);
            id
        };
        #[cfg(brix_parallel)]
        let id = self.next_call.fetch_add(1);
        self.overrides.borrow_mut().insert(id, overrides);
        id
    }

    pub(crate) fn with_override<R>(
        &self,
        call: u64,
        key: &str,
        f: impl FnOnce(&Override) -> R,
    ) -> Option<R> {
        #[cfg(brix_parallel)]
        {
            // The callback can allocate and park for GC. Never retain a host
            // cache lock across it: another active mutator may need that lock
            // before it can reach the same rendezvous.
            let value = self
                .overrides
                .borrow()
                .get(&call)
                .and_then(|m| m.get(key))
                .cloned();
            value.as_ref().map(f)
        }
        #[cfg(not(brix_parallel))]
        self.overrides
            .borrow()
            .get(&call)
            .and_then(|m| m.get(key))
            .map(f)
    }

    /// The flake registries, loaded on first use (`getRegistries`): the
    /// user's (`getConfigDir()/registry.json`), the system's
    /// (`nixConfDir()/registry.json`) and the global one (`flake-registry`).
    pub(crate) fn registries(&self) -> Vec<Registry> {
        if let Some(r) = self.registries.borrow().as_ref() {
            return r.clone();
        }
        let warn = |msg: &str| crate::log::stderr_bytes(&brix_eval::indent_message("warning", msg));
        let user = crate::nixconf::nix_config_dir().join("registry.json");
        let system = crate::nixconf::nix_conf_dir().join("registry.json");
        let registries = vec![
            read_registry_file(&self.settings, &user, RegistryType::User, &warn),
            read_registry_file(&self.settings, &system, RegistryType::System, &warn),
            global_registry(
                &self.settings,
                &self.flake_registry,
                &|url| self.download_registry(url),
                &warn,
            ),
        ];
        *self.registries.borrow_mut() = Some(registries.clone());
        registries
    }

    /// The global registry at `url` (`downloadFile(…, "flake-registry.json")`):
    /// from the fetch cache while it is younger than `tarball-ttl`, else
    /// downloaded and remembered there. A download that fails when an
    /// expired copy is at hand warns and uses that copy, as Nix does.
    fn download_registry(&self, url: &str) -> std::result::Result<Vec<u8>, String> {
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| "fetching is disabled in this session".to_owned())?;
        let cache = self.persistent_cache.as_ref();
        let key: Attrs = [
            ("type".to_owned(), Attr::String("file".into())),
            ("url".to_owned(), Attr::String(url.to_owned())),
            (
                "name".to_owned(),
                Attr::String("flake-registry.json".into()),
            ),
        ]
        .into_iter()
        .collect();
        let ttl = std::time::Duration::from_secs(self.settings.tarball_ttl);
        let cached = |ttl| -> std::result::Result<Option<Vec<u8>>, String> {
            let Some(cache) = cache else {
                return Ok(None);
            };
            let Some(value) = cache.lookup(&key, ttl).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            let Some(Attr::String(hash)) = value.get("narHash") else {
                return Ok(None);
            };
            let hash = brix_flake::NarHash::parse_sri(hash, self.settings.xp())
                .map_err(|e| e.to_string())?;
            match cache.tree_dir(&hash).map_err(|e| e.to_string())? {
                Some(file) => std::fs::read(&file)
                    .map(Some)
                    .map_err(|e| format!("reading '{}': {e}", file.display())),
                None => Ok(None),
            }
        };
        // A TTL of 0 expires every entry (`FetchCache::lookup`, as
        // `Cache::lookupExpired`).
        if let Some(data) = cached(Some(ttl))? {
            return Ok(data);
        }
        let _waiting = Waiting::new(&*self.progress);
        let data = match http.download(url, &[], &self.settings) {
            Ok(download) => download.data,
            Err(e) => {
                return match cached(None)? {
                    Some(data) => {
                        crate::log::stderr(&format!("warning: {e}; using cached version"));
                        Ok(data)
                    }
                    None => Err(e.to_string()),
                }
            }
        };
        if let Some(cache) = cache {
            let file = MemoryAccessor::from_root(brix_store::accessor::MemoryNode::Regular {
                executable: false,
                contents: data.clone(),
            });
            let hash = brix_flake::fetch::tree_nar_hash(&file).map_err(|e| e.to_string())?;
            cache.store_tree(&hash, &file).map_err(|e| e.to_string())?;
            let value: Attrs = [("narHash".to_owned(), Attr::String(hash.to_sri()))]
                .into_iter()
                .collect();
            cache.insert(&key, &value).map_err(|e| e.to_string())?;
        }
        Ok(data)
    }

    /// Whether the configured store has `path` (see [`crate::store::store_has`]).
    pub(crate) fn daemon_has(&self, path: &StorePath) -> Result<bool> {
        crate::store::store_has(&self.daemon, path)
    }
}

/// `--fetch-cache-dir` or brix's default cache directory.
fn fetch_cache_dir(config: &Config) -> Result<std::path::PathBuf> {
    match &config.fetch.cache_dir {
        Some(dir) => Ok(dir.clone()),
        None => Ok(brix_flake::native::default_cache_dir()?),
    }
}

/// A fetcher's warning, through the logger: Nix's fetchers warn with
/// `warn()` (`Logger::warn`), which writes `warning: ` and the message
/// as it is, continuation lines unindented (unlike `logWarning`'s
/// `showErrorInfo`).
fn log_warning(msg: &str) {
    crate::log::stderr(&format!("warning: {msg}"));
}

/// The HTTP client downloads and Git fetches go through: trusting
/// `ssl-cert-file`, reporting to the session's progress and logger.
fn http_client(config: &Config) -> brix_flake::native::http::HttpClient {
    brix_flake::native::http::HttpClient::with_ca_file(config.fetch.ssl_cert_file.as_deref())
        .with_progress(config.progress.clone())
        .with_warn(Arc::new(log_warning))
}

fn native_fetcher(config: &Config) -> Result<brix_flake::NativeFetcher> {
    let mut fetcher = brix_flake::NativeFetcher::new()?
        .with_cache_dir(fetch_cache_dir(config)?)
        .with_progress(config.progress.clone());
    // The host checks the local store itself (so it can ask the daemon),
    // but tells the fetcher about it so trees the store already has are
    // not copied into the cache as well.
    fetcher.store_with_trees = Some(config.store_dir().clone());
    fetcher.warn = Box::new(log_warning);
    fetcher.http = http_client(config);
    fetcher.cache = fetcher
        .cache
        .map(|cache| cache.verifying(config.fetch.verify_cache));
    Ok(fetcher)
}

/// Nix's builtin copy of the global flake registry
/// (`src/libfetchers/builtin-flake-registry.json`), used when the
/// configured one cannot be fetched.
const BUILTIN_FLAKE_REGISTRY: &str = include_str!("builtin-flake-registry.json");

/// `Registry::read(settings, path, type)`: the registry in a file, empty
/// when there is no such file. A file that cannot be read or parsed is
/// an empty registry too, with a warning, as in Nix.
fn read_registry_file(
    settings: &FetchSettings,
    path: &std::path::Path,
    registry_type: RegistryType,
    warn: &dyn Fn(&str),
) -> Registry {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::empty(registry_type),
        Err(e) => {
            warn(&format!(
                "cannot read flake registry '{}': {e}",
                path.display()
            ));
            Registry::empty(registry_type)
        }
        // Nix warns about a registry it cannot parse and keeps what it read.
        Ok(text) => Registry::read_file(
            settings,
            &path.display().to_string(),
            &text,
            registry_type,
            warn,
        ),
    }
}

/// `getGlobalRegistry`: none for an empty `flake-registry`, the file for
/// an absolute path, else the download of the URL. A download that
/// fails is replaced by Nix's builtin registry, with a warning.
fn global_registry(
    settings: &FetchSettings,
    location: &str,
    download: &dyn Fn(&str) -> std::result::Result<Vec<u8>, String>,
    warn: &dyn Fn(&str),
) -> Registry {
    if location.is_empty() {
        return Registry::empty(RegistryType::Global);
    }
    if std::path::Path::new(location).is_absolute() {
        return read_registry_file(
            settings,
            std::path::Path::new(location),
            RegistryType::Global,
            warn,
        );
    }
    let data = match download(location) {
        Ok(data) => data,
        Err(e) => {
            warn(&format!(
                "cannot fetch global flake registry '{location}', will use builtin fallback registry: {e}"
            ));
            return Registry::parse(
                settings,
                "<builtin>",
                BUILTIN_FLAKE_REGISTRY,
                RegistryType::Global,
                warn,
            )
            .expect("Nix's builtin flake registry parses");
        }
    };
    match String::from_utf8(data) {
        Ok(text) => Registry::read_file(settings, location, &text, RegistryType::Global, warn),
        Err(_) => {
            warn(&format!(
                "cannot read flake registry '{location}': it is not UTF-8"
            ));
            Registry::empty(RegistryType::Global)
        }
    }
}

/// `fetchers::Attrs` → the evaluator's attribute form.
pub(crate) fn attrs_to_fetch(attrs: &Attrs) -> BTreeMap<String, FetchAttr> {
    attrs
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Attr::String(s) => FetchAttr::String(s.clone()),
                Attr::Int(n) => FetchAttr::Int(*n),
                Attr::Bool(b) => FetchAttr::Bool(*b),
            };
            (k.clone(), v)
        })
        .collect()
}

/// A failed fetch as the evaluator shows it: a fetcher's error with its
/// traces ("while fetching the input '…'", "while smudging git-lfs file
/// '…'") and exit status; any other error's text.
fn fetch_error(e: &Error) -> FetchError {
    match e {
        Error::Flake(e) => flake_fetch_error(e),
        other => FetchError::new(other.to_string()),
    }
}

fn flake_fetch_error(e: &brix_flake::Error) -> FetchError {
    FetchError {
        message: e.message(),
        traces: e.traces().into_iter().map(str::to_owned).collect(),
        exit_status: e.exit_status(),
        foreign: e.is_foreign(),
    }
}

/// The evaluator's attribute form → `fetchers::Attrs`.
pub(crate) fn fetch_to_attrs(attrs: &BTreeMap<String, FetchAttr>) -> Attrs {
    attrs
        .iter()
        .map(|(k, v)| {
            let v = match v {
                FetchAttr::String(s) => Attr::String(s.clone()),
                FetchAttr::Int(n) => Attr::Int(*n),
                FetchAttr::Bool(b) => Attr::Bool(*b),
            };
            (k.clone(), v)
        })
        .collect()
}

impl Host for NativeHost {
    /// `builtins.trace`, warnings and the parser's lint findings go
    /// through the logger: ANSI codes are filtered like Nix's
    /// `SimpleLogger` does, once per message.
    fn log(&self, text: &[u8]) {
        crate::log::stderr_bytes(text);
    }

    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn compiled_image(
        &self,
        tree: &FetchedTree,
        ctx: &brix_eval::image::ImageCompileContext,
    ) -> std::result::Result<Option<Arc<brix_eval::image::Image>>, String> {
        self.images.image_for(tree, ctx)
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        self.allowed.clone()
    }

    fn fetch_tree(&self, request: &FetchRequest) -> std::result::Result<FetchedTree, FetchError> {
        let mut input = match (request.attrs.get("type"), request.attrs.get("url")) {
            // `fetchTree "<url>"`: `Input::fromURL`.
            (None, Some(FetchAttr::String(url))) => Input::from_url_str(&self.settings, url, true),
            _ => Input::from_attrs(&self.settings, fetch_to_attrs(&request.attrs)),
        }
        .map_err(|e| flake_fetch_error(&e))?;
        // `input.to_string()`, which Nix prints before anything is fetched
        // (in the pure-mode check's message, and for `checkURI`): where a
        // Git input's `publicKeys` that is not JSON fails.
        let url = input
            .to_url_text(false)
            .map_err(|e| flake_fetch_error(&e))?;
        // Pure evaluation (brix's only mode) fetches locked inputs, and an
        // unlocked one with a NAR hash after a warning (`fetchTree`).
        let locked = input
            .is_locked(&self.settings)
            .map_err(|e| flake_fetch_error(&e))?;
        let nar_hash = input.nar_hash().map_err(|e| flake_fetch_error(&e))?;
        if !locked {
            if nar_hash.is_none() {
                return Err(FetchError::new(format!(
                    "in pure evaluation mode, '{}' doesn't fetch unlocked input '{url}'",
                    request.who,
                )));
            }
            log_warning(&format!(
                "Input '{url}' is unlocked (e.g. lacks a Git revision) but is checked by NAR hash. This is not reproducible and will break after garbage collection or when shared."
            ));
        }
        if request.nar_hash_is_final && nar_hash.is_some() {
            input.set_final();
        }
        if request.refetch {
            // Past the input cache (see `FetchRequest::refetch`).
            self.fetch_cache.borrow_mut().remove(&input.attrs);
        }
        let tree = self
            .fetch_locked(&input, true)
            .map_err(|e| fetch_error(&e))?;
        self.to_eval_tree(&tree, request.empty_rev_fallback)
            .map_err(|e| fetch_error(&e))
    }

    fn fetch_file(&self, url: &str, name: &str) -> std::result::Result<FetchedFile, String> {
        let http = self.http.as_ref().ok_or_else(|| {
            format!("fetching is disabled in this session (while downloading '{url}')")
        })?;
        let _waiting = Waiting::new(&*self.progress);
        let download = http
            .download(url, &[], &self.settings)
            .map_err(|e| e.to_string())?;
        let hash = Hash::of(brix_store::HashAlgo::Sha256, &download.data);
        let store_path = self
            .store_dir
            .make_fixed_output_path(
                name,
                &brix_store::FixedOutputInfo {
                    method: brix_store::FileIngestionMethod::Flat,
                    hash,
                    references: Default::default(),
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(FetchedFile {
            contents: download.data,
            store_path,
        })
    }

    fn build(
        &self,
        requests: &[BuildRequest],
        view: &dyn EvalStoreView,
    ) -> std::result::Result<Vec<StorePath>, String> {
        let _waiting = Waiting::new(&*self.progress);
        let _activity = brix_flake::Activity::new(
            &*self.progress,
            brix_flake::ActivityKind::Other,
            "building during evaluation",
        );
        let mut daemon = self.daemon.lock().expect("daemon state poisoned");
        let client = daemon
            .client(&format!(
                "building '{}' during evaluation (import-from-derivation)",
                requests
                    .first()
                    .map(|r| self.store_dir.display(&r.drv_path))
                    .unwrap_or_default()
            ))
            .map_err(|e| e.to_string())?;
        crate::store::daemon_build::build(
            client,
            &self.store_dir,
            &self.root,
            &self.trees,
            requests,
            view,
        )
        .map_err(|e| e.to_string())
    }

    /// Trees this host mounted exist by construction; anything else is
    /// the daemon's call.
    fn is_valid_path(&self, path: &StorePath) -> std::result::Result<bool, String> {
        if self.trees.borrow().contains_key(path) {
            return Ok(true);
        }
        self.daemon_has(path).map_err(|e| e.to_string())
    }
}

/// Where Nix shows a tree's files: a `path` input and a local git checkout
/// (`git+file:`) are shown at their real directory, as their accessors are
/// the file system itself; anything fetched is shown as `«locked url»/…`.
fn local_display_prefix(input: &Input) -> Option<String> {
    match input.type_name().ok()? {
        "path" => match input.attrs.get("path") {
            Some(Attr::String(p)) => Some(p.trim_end_matches('/').to_owned()),
            _ => None,
        },
        "git" => match input.attrs.get("url") {
            Some(Attr::String(url)) => url
                .strip_prefix("file://")
                .map(|p| p.trim_end_matches('/').to_owned()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const REGISTRY: &str = r#"{"version": 2, "flakes": [{"from": {"type": "indirect", "id": "mine"}, "to": {"type": "github", "owner": "me", "repo": "mine"}}]}"#;

    fn ids(r: &Registry) -> Vec<String> {
        r.entries
            .iter()
            .map(|e| match e.from.attrs.get("id") {
                Some(Attr::String(id)) => id.clone(),
                other => panic!("an indirect entry has an id: {other:?}"),
            })
            .collect()
    }

    /// Collects warnings instead of printing them.
    #[derive(Default)]
    struct Warnings(RefCell<Vec<String>>);

    impl Warnings {
        fn warn(&self) -> impl Fn(&str) + '_ {
            |msg| self.0.borrow_mut().push(msg.to_owned())
        }

        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.borrow_mut())
        }
    }

    /// `Registry::read`: no file is an empty registry; a file that
    /// cannot be read or parsed is one too, with a warning.
    #[test]
    fn registry_files_read_like_nix() {
        let settings = FetchSettings::default();
        let dir = tempfile::tempdir().unwrap();
        let warnings = Warnings::default();
        let read = |name: &str| {
            read_registry_file(
                &settings,
                &dir.path().join(name),
                RegistryType::User,
                &warnings.warn(),
            )
        };
        assert_eq!(read("absent.json"), Registry::empty(RegistryType::User));
        assert!(warnings.take().is_empty());
        std::fs::write(dir.path().join("good.json"), REGISTRY).unwrap();
        assert_eq!(ids(&read("good.json")), ["mine"]);
        assert!(warnings.take().is_empty());
        std::fs::write(dir.path().join("bad.json"), "{ not json").unwrap();
        assert_eq!(read("bad.json"), Registry::empty(RegistryType::User));
        let w = warnings.take();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].starts_with(&format!(
                // `Registry::read` on the JSON: Nix's "cannot parse".
                "cannot parse flake registry '{}': ",
                dir.path().join("bad.json").display()
            )),
            "{w:?}"
        );
        // A directory cannot be read as a file.
        std::fs::create_dir(dir.path().join("dir.json")).unwrap();
        assert_eq!(read("dir.json"), Registry::empty(RegistryType::User));
        assert_eq!(warnings.take().len(), 1);
    }

    /// `getGlobalRegistry`: `flake-registry` empty, a path, or a URL; a
    /// failed download falls back to the builtin registry with Nix's
    /// warning.
    #[test]
    fn the_global_registry_comes_from_flake_registry() {
        let settings = FetchSettings::default();
        let warnings = Warnings::default();
        let no_download = |url: &str| -> std::result::Result<Vec<u8>, String> {
            panic!("'{url}' should not be downloaded")
        };
        let global =
            |location: &str, download: &dyn Fn(&str) -> std::result::Result<Vec<u8>, String>| {
                global_registry(&settings, location, download, &warnings.warn())
            };
        assert_eq!(
            global("", &no_download),
            Registry::empty(RegistryType::Global)
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("registry.json");
        std::fs::write(&file, REGISTRY).unwrap();
        let from_file = global(file.to_str().unwrap(), &no_download);
        assert_eq!(from_file.registry_type, RegistryType::Global);
        assert_eq!(ids(&from_file), ["mine"]);
        // A path that does not exist is an empty registry, not a failure.
        let absent = dir.path().join("absent.json");
        assert_eq!(
            global(absent.to_str().unwrap(), &no_download),
            Registry::empty(RegistryType::Global)
        );
        assert!(warnings.take().is_empty());
        let downloaded = global("https://example.org/registry.json", &|url| {
            assert_eq!(url, "https://example.org/registry.json");
            Ok(REGISTRY.as_bytes().to_vec())
        });
        assert_eq!(ids(&downloaded), ["mine"]);
        assert!(warnings.take().is_empty());
        let fallback = global("https://example.org/gone.json", &|_| {
            Err("HTTP error 404".to_owned())
        });
        assert_eq!(fallback.registry_type, RegistryType::Global);
        assert!(ids(&fallback).iter().any(|id| id == "nixpkgs"));
        assert_eq!(
            warnings.take(),
            ["cannot fetch global flake registry 'https://example.org/gone.json', will use builtin fallback registry: HTTP error 404"]
        );
    }

    /// The global registry is downloaded once per `tarball-ttl`, then read
    /// from the fetch cache (`downloadFile`); past it, a failed download
    /// falls back on the cached copy with Nix's warning.
    #[test]
    fn the_global_registry_is_cached_for_tarball_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("registry.json");
        std::fs::write(&file, REGISTRY).unwrap();
        let url = format!("file://{}", file.display());
        let global = |ttl: u64| {
            let mut config = Config::default();
            config.fetch.cache_dir = Some(dir.path().join("cache"));
            config.fetch.flake_registry = url.clone();
            config.fetch.tarball_ttl = ttl;
            let host = NativeHost::new(&config).unwrap();
            let registries = host.registries();
            ids(registries.last().expect("the global registry comes last"))
        };
        assert_eq!(global(3600), ["mine"]);
        std::fs::write(
            &file,
            REGISTRY.replace(r#""id": "mine""#, r#""id": "theirs""#),
        )
        .unwrap();
        assert_eq!(global(3600), ["mine"], "served from the cache");
        assert_eq!(global(0), ["theirs"], "expired: downloaded again");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(global(0), ["theirs"], "the cached copy, with a warning");
    }

    /// The builtin fallback is Nix's own file.
    #[test]
    fn the_builtin_registry_is_nixs() {
        let nixs = std::fs::read_to_string(
            brix_testutil::nix_src().join("src/libfetchers/builtin-flake-registry.json"),
        )
        .expect("the reference sources have a builtin flake registry");
        assert_eq!(BUILTIN_FLAKE_REGISTRY, nixs);
    }
}
