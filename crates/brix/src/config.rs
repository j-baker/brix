//! Session configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use brix_flake::{NoProgress, Progress};
use brix_store::StoreDir;

/// How fetching works.
#[derive(Clone, Debug)]
pub struct FetchConfig {
    /// Fetch inputs over the network and from the file system. With
    /// `false`, every fetch is an error (hermetic evaluation).
    pub enabled: bool,
    /// `access-tokens`: `host` or `host/owner/repo` → token, for the
    /// GitHub/GitLab/SourceHut APIs.
    pub access_tokens: BTreeMap<String, String>,
    /// Where Git clones are cached. `None` uses `$XDG_CACHE_HOME/brix`.
    pub cache_dir: Option<PathBuf>,
    /// `tarball-ttl`: how long cached unlocked fetches (branches, tarball
    /// URLs) stay fresh, in seconds. `nix --refresh` sets it to 0, `nix
    /// --offline` to `u32::MAX` unless the configuration sets it.
    pub tarball_ttl: u64,
    /// `ssl-cert-file`: the CA certificates HTTPS transfers (downloads
    /// and Git fetches) trust, and only those. The default is Nix's:
    /// `NIX_SSL_CERT_FILE`, `SSL_CERT_FILE` or the first system bundle
    /// that exists. `None` leaves the choice to the HTTP client, as Nix
    /// leaves it to curl (see `brix_flake::native::http::HttpClient`).
    pub ssl_cert_file: Option<PathBuf>,
    /// `netrc-file`: credentials sent as basic authentication to the hosts
    /// it names (private FlakeHub inputs, for instance). `None` sends none.
    pub netrc_file: Option<PathBuf>,
    /// `allow-dirty`: whether a Git working tree with uncommitted changes
    /// may be used at all.
    pub allow_dirty: bool,
    /// `warn-dirty`: warn when one is.
    pub warn_dirty: bool,
    /// `allow-dirty-locks`: a lock entry pinned only by its `narHash`
    /// counts as locked.
    pub allow_dirty_locks: bool,
    /// `trust-tarballs-from-git-forges`: a forge input (`github:` …) with a
    /// `rev` counts as locked without a `narHash`.
    pub trust_tarballs_from_git_forges: bool,
    /// `nix-219-compat`: keep `?dir=` in the `url` of inputs that have one,
    /// and export Git commits with `git archive`, as Nix < 2.20 did.
    pub nix219_compat: bool,
    /// `filetransfer-retry-attempts` (alias `download-attempts`): how many
    /// times a download that fails transiently is tried in all.
    pub download_attempts: u32,
    /// `filetransfer-retry-delay`: the longest delay before the first retry
    /// of a download, in milliseconds, doubled for each further one.
    pub download_retry_delay_ms: u32,
    /// `connect-timeout`: how many seconds a download may spend connecting
    /// (resolving, connecting, the TLS handshake) before the attempt
    /// fails; 0 for no limit.
    pub connect_timeout: u64,
    /// `http-connections`: how many inputs may be fetched at once (brix
    /// fetches the inputs it can foresee needing in the background; see
    /// `fetching.rs`). 0 means one per CPU.
    pub http_connections: usize,
    /// `flake-registry`: the global flake registry, a URL to download or
    /// an absolute path to read; empty for none. When it cannot be
    /// downloaded, Nix's builtin copy is used, with a warning.
    pub flake_registry: String,
    /// Rehash a cached fetched tree whenever it is used, rather than
    /// trusting the cache (the CLI sets it from `BRIX_FETCH_CACHE_VERIFY`).
    pub verify_cache: bool,
    /// Whether Nix's notices about flake references are shown (`notice`
    /// is within the verbosity), such as the one saying that a directory
    /// without a `flake.nix` is searched upwards.
    pub notices: bool,
    /// Whether the fetch activities Nix starts at the `info` level are
    /// announced on stderr (`info` is within the verbosity), such as
    /// "unpacking '…' into the Git cache...": Nix's default off a
    /// terminal, where its progress bar shows them instead.
    pub infos: bool,
}

/// `flake-registry`'s default in Determinate Nix (`fetch-settings.hh`).
pub const DEFAULT_FLAKE_REGISTRY: &str =
    "https://install.determinate.systems/flake-registry/stable/flake-registry.json";

impl Default for FetchConfig {
    fn default() -> Self {
        FetchConfig {
            enabled: true,
            access_tokens: BTreeMap::new(),
            cache_dir: None,
            tarball_ttl: 3600,
            ssl_cert_file: brix_flake::native::http::default_ssl_cert_file(),
            netrc_file: None,
            allow_dirty: true,
            warn_dirty: true,
            allow_dirty_locks: false,
            trust_tarballs_from_git_forges: true,
            nix219_compat: false,
            download_attempts: 5,
            download_retry_delay_ms: 100,
            // Nix's defaults.
            connect_timeout: 15,
            http_connections: 25,
            flake_registry: DEFAULT_FLAKE_REGISTRY.to_owned(),
            verify_cache: false,
            notices: true,
            infos: true,
        }
    }
}

/// Compiled flake-input images (see `brix_eval::image`): inputs pinned by
/// a `narHash` can be served from a precompiled image instead of being
/// parsed on every evaluation.
#[derive(Clone, Debug)]
pub struct CompiledInputsConfig {
    /// Use images from the cache, and compile inputs that have none.
    pub enabled: bool,
    /// Where images live: `<dir>/compiled/`. `None` uses the fetch cache's
    /// directory (`$XDG_CACHE_HOME/brix`).
    pub cache_dir: Option<PathBuf>,
    /// Images supplied explicitly, keyed by the input's `narHash` (SRI or
    /// nix32) or, once a flake is loaded, by an input name of its lock
    /// (`nixpkgs`). They are used in preference to the cache and are never
    /// written to.
    pub explicit: Vec<(String, PathBuf)>,
    /// Only trees with at least this many `.nix` files are compiled on
    /// first use; smaller ones are cheaper to parse than to image.
    pub min_nix_files: usize,
}

impl Default for CompiledInputsConfig {
    fn default() -> Self {
        CompiledInputsConfig {
            enabled: true,
            cache_dir: None,
            explicit: Vec::new(),
            min_nix_files: 50,
        }
    }
}

/// Everything a [`crate::Session`] needs. `Default` is a hermetic
/// evaluator: fetching on, no daemon, no paths outside the store.
#[derive(Clone, Debug)]
pub struct Config {
    /// Evaluator settings; `eval.store_dir` is the store directory.
    pub eval: brix_eval::Config,
    /// Precise runtime heap budget and optional collection stress mode.
    pub gc: brix_eval::managed_api::GcConfig,
    /// The Nix store brix builds in and queries: a daemon URI (`daemon`,
    /// `unix:///path`, `ssh-ng://[user@]host`). `None` means no store at
    /// all (`dummy://`): evaluation works, anything needing a build or a
    /// store query fails loudly. The connection is made on first use.
    pub daemon: Option<String>,
    /// What to print while the daemon builds.
    pub build_log: BuildLogMode,
    /// Optional terminal renderer for structured daemon activities.
    pub build_progress: Option<Arc<dyn crate::store::BuildProgress>>,
    /// `nix.conf` settings to send to the daemon (`max-jobs`,
    /// `keep-going`, …), as `nix --option` would.
    pub daemon_settings: BTreeMap<String, String>,
    /// Directories outside the store that expressions may read, as
    /// `--allowed-paths` would allow them in pure mode.
    pub allowed_paths: Vec<PathBuf>,
    pub fetch: FetchConfig,
    pub compiled: CompiledInputsConfig,
    /// Where fetches report their progress (downloads, Git fetches). The
    /// command line draws Nix's progress bar from it.
    pub progress: Arc<dyn Progress>,
    /// What dropping the session does with the evaluator's memory.
    pub teardown: Teardown,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            eval: brix_eval::Config {
                // Nix's CLI default; the test suites set it explicitly.
                show_trace: false,
                ..brix_eval::Config::default()
            },
            gc: brix_eval::managed_api::GcConfig {
                // A large startup nursery, then smaller steady-state collections.
                // Retained old objects do not consume the nursery budget.
                // This is a soft trigger, not a process memory limit.
                // Cap at usize::MAX on targets that cannot represent 4 GiB.
                threshold_bytes: 4usize.saturating_mul(1024 * 1024 * 1024),
                steady_threshold_bytes: Some(512 * 1024 * 1024),
                // Large evaluations reuse the CLI allocator's existing pre-faulting.
                chunk_bytes: 2 * 1024 * 1024,
                ..Default::default()
            },
            daemon: None,
            build_log: BuildLogMode::default(),
            build_progress: None,
            daemon_settings: BTreeMap::new(),
            allowed_paths: Vec::new(),
            fetch: FetchConfig::default(),
            compiled: CompiledInputsConfig::default(),
            progress: Arc::new(NoProgress),
            teardown: Teardown::default(),
        }
    }
}

impl Config {
    pub fn store_dir(&self) -> &StoreDir {
        &self.eval.store_dir
    }
}

/// Nix's `thisSystem` for the machine brix runs on.
pub fn host_system() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{}-{}", std::env::consts::ARCH, os)
}

/// How much of the daemon's output reaches stderr during a build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildLogMode {
    /// Nothing but errors and warnings.
    Quiet,
    /// Nix's default: "building '/nix/store/…drv'…" notices, warnings,
    /// and the daemon's messages at the `info` level.
    #[default]
    Normal,
    /// `nix -L`: build output lines too, each prefixed with the
    /// derivation's name.
    Logs,
}

/// What dropping a [`crate::Session`] does with the evaluator's memory:
/// the arena every value lives in and the evaluator's own tables
/// (symbols, parsed files, the virtual store's derivations).
///
/// Either way, dropping the session releases its resources: the daemon
/// connection is closed (an `ssh` child is killed; for `ssh-ng://localhost`
/// the local `nix-daemon --stdio` gets up to 10 s to exit first) and
/// `BRIX_TIMINGS` are printed. Nothing the session writes to disk (fetch
/// caches, compiled images, lock files) waits for the drop; each is
/// written when it is made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Teardown {
    /// Return the memory to the allocator, for a program that goes on
    /// after the session.
    #[default]
    Free,
    /// Leave the memory allocated, for a process that exits as soon as
    /// the session is done (the `brix` binary): the operating system takes
    /// it back at exit in one go, where freeing it walks every table and
    /// has the allocator hand the arena back to the kernel a few pages at
    /// a time. That took 2-4% off the wall time of a firefox evaluation
    /// through the CLI.
    Leak,
}
