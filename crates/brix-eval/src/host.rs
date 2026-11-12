//! The evaluator's view of the outside world.
//!
//! brix-eval does no I/O of its own. Everything that reaches outside the
//! expression being evaluated goes through a [`Host`]: the file system (as
//! a [`SourceAccessor`]), fetchers, and the builder used by
//! import-from-derivation. The defaults refuse everything, so a plain
//! evaluator is hermetic.

use std::collections::BTreeMap;
use std::sync::Arc;

use brix_store::{CanonPath, SourceAccessor, StorePath};

/// An argument to `builtins.fetchTree` and friends, after Nix's
/// coercion of the attribute set (`fetchers::Attrs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchAttr {
    String(String),
    Bool(bool),
    Int(u64),
}

/// A fetch request. `attrs` contains `type`, except for `fetchTree`
/// given a URL rather than attributes, where it is the `url` alone, for
/// the host to read as a URL (`Input::fromURL`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    pub attrs: BTreeMap<String, FetchAttr>,
    /// Which builtin asked (`fetchTree`, `fetchGit`, `fetchTarball`,
    /// `fetchurl`, `fetchMercurial`), for error messages.
    pub who: &'static str,
    /// `emitTreeAttrs`'s `emptyRevFallback` (`fetchGit`): a tree without
    /// a revision reports the all-zero one and no commits.
    pub empty_rev_fallback: bool,
    /// `fetchTree`'s rule (`fetchTree` and `fetchGit`): an input with a
    /// `narHash` is final, so the tree is described by the input's own
    /// attributes, which those the fetcher reports must agree with
    /// (`Input::checkLocks`). `fetchTarball` and `fetchMercurial` fetch
    /// their input as it is.
    pub nar_hash_is_final: bool,
    /// Fetch even if this evaluation fetched the same input before.
    /// `fetchMercurial` calls `Input::fetchToStore` itself, past the input
    /// cache that `fetchTree`, `fetchTarball` and flakes share, so each of
    /// its calls fetches again; and Nix's Mercurial fetcher answers a
    /// second fetch of a revision from its own cache, reporting the `ref`
    /// asked for rather than the revision's branch. A host that kept the
    /// first result would report the branch twice.
    pub refetch: bool,
}

/// Why a fetch failed, as Nix's error says it: the message, the "while
/// …" lines the fetchers added to it (`addTrace`: "while fetching the
/// input '…'", "while smudging git-lfs file '…'"), and the exit status
/// the process ends with. The evaluator puts the traces on its error,
/// below those it adds itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchError {
    pub message: String,
    /// Outermost (added last) first, the order Nix prints them in.
    pub traces: Vec<String>,
    /// 102 for a hash mismatch (`Error((unsigned int) 102, …)`), else 1.
    pub exit_status: u32,
    /// An exception that is not a Nix `Error` (nlohmann's, for a JSON
    /// document a fetcher reads): Nix prints it alone, never traced
    /// ([`crate::ErrorKind::Foreign`]).
    pub foreign: bool,
}

impl FetchError {
    /// An error without traces, ending the process with 1.
    pub fn new(message: impl Into<String>) -> FetchError {
        FetchError {
            message: message.into(),
            traces: Vec::new(),
            exit_status: 1,
            foreign: false,
        }
    }
}

impl From<String> for FetchError {
    fn from(message: String) -> FetchError {
        FetchError::new(message)
    }
}

impl From<&str> for FetchError {
    fn from(message: &str) -> FetchError {
        FetchError::new(message)
    }
}

impl std::fmt::Display for FetchError {
    /// The message, without traces (Nix's `msg()`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A fetched source tree, as the fetcher reports it.
pub struct FetchedTree {
    /// The tree; it is mounted at `store_path` on the root accessor.
    pub accessor: Arc<dyn SourceAccessor>,
    /// The store path of the tree (`makeFixedOutputPath(NixArchive,
    /// narHash, name)`), which the host has already computed.
    pub store_path: StorePath,
    /// The attributes `builtins.fetchTree` returns besides `outPath`, as
    /// Nix's `emitTreeAttrs` computes them from the locked input
    /// (`narHash`, `rev`, `shortRev`, `revCount`, `lastModified`,
    /// `lastModifiedDate`, …). The host computes them; the evaluator only
    /// turns them into values.
    pub attrs: BTreeMap<String, FetchAttr>,
    /// The locked input's own attributes (Nix's `Input::attrs` after the
    /// fetch: `type`, `url`, `ref`, `rev`, `revCount`, …), for a builtin
    /// that shapes its result from the input rather than with
    /// `emitTreeAttrs`: `fetchMercurial` returns its `ref` as `branch`.
    pub locked_attrs: BTreeMap<String, FetchAttr>,
    /// The locked input in URL form, for messages.
    pub locked_url: String,
    /// How paths inside the tree are shown in positions and messages.
    /// Nix shows a local tree (a `path` input, a local git checkout) at its
    /// real directory and anything fetched as `«locked url»`; `None`
    /// means the latter.
    pub display_prefix: Option<String>,
}

/// A fetched file (`fetchurl` without unpacking).
pub struct FetchedFile {
    pub contents: Vec<u8>,
    pub store_path: StorePath,
}

/// What a real store registers a source object of the virtual store as,
/// known before its NAR is written: what a host that uploads it (before a
/// build, or for import-from-derivation) announces it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceNar {
    /// SHA-256 of the NAR.
    pub hash: brix_store::Hash,
    pub size: u64,
    /// The content address the store path was made from.
    pub ca: brix_store::ContentAddress,
}

/// The evaluation state a [`Host::build`] implementation may consult: the
/// virtual store's objects (derivations and sources the daemon has never
/// seen) and their contents. Implemented by the evaluator.
pub trait EvalStoreView {
    /// The store object brix computed for `path`, if any.
    fn object(&self, path: &StorePath) -> Option<brix_store::StoreObject>;
    /// The NAR of a source object (not a derivation or text object,
    /// whose bytes are in the [`brix_store::StoreObject`]) as the store
    /// holds it, without serialising it: file sizes are taken from the
    /// accessor where it knows them.
    fn source_nar(&self, path: &StorePath) -> Result<SourceNar, String>;
    /// Writes that NAR to `out`, reading the source as it goes.
    fn write_source_nar(
        &self,
        path: &StorePath,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String>;
}

/// A derivation output that evaluation needs built. Output names are
/// bytes, as a string context holds them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRequest {
    pub drv_path: StorePath,
    pub outputs: Vec<bstr::BString>,
}

pub trait Host: crate::runtime::ThreadSafe {
    /// The root file system. It is registered as accessor 0 and every
    /// path value lives on it; the host decides what `/` contains (an
    /// empty tree in pure mode, with store paths and fetched trees mounted
    /// under the store directory).
    fn root_accessor(&self) -> Arc<dyn SourceAccessor>;

    /// Paths that may be read in pure mode, with everything below them,
    /// besides what evaluation adds to the store.
    fn allowed_paths(&self) -> Vec<CanonPath> {
        Vec::new()
    }

    /// Fetches a tree for `fetchTree`/`fetchGit`/`fetchTarball`/
    /// `fetchMercurial`.
    fn fetch_tree(&self, request: &FetchRequest) -> Result<FetchedTree, FetchError> {
        Err(FetchError::new(format!(
            "'{}' is not supported by this host (no fetcher configured)",
            request.who
        )))
    }

    /// Downloads a file for `fetchurl` (no unpacking).
    fn fetch_file(&self, url: &str, name: &str) -> Result<FetchedFile, String> {
        let _ = name;
        Err(format!(
            "'fetchurl' of '{url}' is not supported by this host (no fetcher configured)"
        ))
    }

    /// Builds derivation outputs (import-from-derivation) and makes them
    /// readable under the store directory on the root accessor. Returns
    /// the closures of the outputs built (`computeFSClosure` of each), the
    /// store paths an import from them may read: the evaluator allows
    /// those, as Nix's `realiseContext` does (`allowClosure`).
    fn build(
        &self,
        requests: &[BuildRequest],
        view: &dyn EvalStoreView,
    ) -> Result<Vec<StorePath>, String> {
        let _ = view;
        Err(format!(
            "cannot build '{}' during evaluation: this host has no builder",
            requests
                .first()
                .map(|r| r.drv_path.to_string())
                .unwrap_or_default()
        ))
    }

    /// Whether a store path is valid in the host's store (`isValidPath`,
    /// for `realiseContext`; the evaluator checks its virtual store
    /// first). `Err` is a store that cannot answer (an unreachable
    /// daemon), reported as the evaluation error. The default has no
    /// store, in which nothing is valid.
    fn is_valid_path(&self, path: &StorePath) -> std::result::Result<bool, String> {
        let _ = path;
        Ok(false)
    }

    /// A compiled image of a fetched tree, if the host has one (see
    /// [`crate::image`]). Called when the tree is mounted; `ctx` says what
    /// the image must have been compiled with. The default has none.
    fn compiled_image(
        &self,
        tree: &FetchedTree,
        ctx: &crate::image::ImageCompileContext,
    ) -> std::result::Result<Option<Arc<crate::image::Image>>, String> {
        let _ = (tree, ctx);
        Ok(None)
    }

    /// Logs one message, laid out as Nix's logger writes it (prefix
    /// included, no final newline): a `trace:` line, a warning, the
    /// parser's lint findings with their code lines. Messages are bytes,
    /// as they quote values. The default writes the message and a newline
    /// to stderr; a host that has a logger of its own implements this,
    /// and [`Host::trace`] and [`Host::warn`] come through it.
    fn log(&self, text: &[u8]) {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        // As Nix's `writeToStderr` (`noexcept`): a message that cannot be
        // written is lost.
        let _ = stderr
            .write_all(text)
            .and_then(|()| stderr.write_all(b"\n"));
    }

    /// `builtins.trace` output: `trace: ` and the message (a string's
    /// bytes, or the value printed).
    fn trace(&self, msg: &[u8]) {
        self.log(&[b"trace: ", msg].concat());
    }

    /// Nix's `warn()` (`Logger::warn`): `warning: ` and the message, its
    /// further lines not indented. Warnings Nix logs with `logWarning`
    /// instead (`builtins.warn`, a missing search path entry) are laid out
    /// like errors ([`crate::indent_message`]) and logged with
    /// [`Host::log`].
    fn warn(&self, msg: &[u8]) {
        self.log(&[b"warning: ", msg].concat());
    }
}

/// A host with an empty root file system and nothing else.
pub struct NullHost;

impl Host for NullHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        Arc::new(brix_store::MemoryAccessor::new())
    }
}
