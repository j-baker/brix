//! The evaluator state: arenas, loaded code, symbols, constants, caches.

use crate::runtime::cell::{Cell, RefCell};
use crate::runtime::rc::Rc;
use crate::runtime::Bump;
use std::collections::{BTreeSet, HashMap};

use brix_store::{
    AccessorId, AccessorRegistry, CanonPath, SourceAccessor, SourcePath, StoreDir, StorePath,
    VirtualStore,
};
use bstr::BString;

use crate::code::LinkedCode;
use crate::compile::{self, StaticEnv};
use crate::error::{msg, ErrorKind, EvalError, Position, SourceInfo};
use crate::host::Host;
use crate::ir::{Code, Node, Origin, Pos};
use crate::string_context::StringContext;
use crate::symbol::{Symbol, SymbolKeys, SymbolTable};
use crate::value::{
    App, Attr, Bindings, CodeId, ContextElem, Env, GlobalNode, List, NixStr, PathVal, PrimOp,
    PrimOpApp, SrcPos, Value, ValueCell, ValueRef,
};
use crate::Result;

/// Evaluator settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// The store directory (`builtins.storeDir`).
    pub store_dir: StoreDir,
    /// `max-call-depth`.
    pub max_call_depth: u32,
    /// Whether "while calling 'f'" / "from call site" traces are added
    /// (`--show-trace`, on by default like Nix's test suite).
    pub show_trace: bool,
    /// `builtins.traceVerbose` prints when set.
    pub trace_verbose: bool,
    /// `allow-import-from-derivation`.
    pub allow_import_from_derivation: bool,
    /// Enabled experimental features (`ca-derivations`,
    /// `dynamic-derivations`, `parse-toml-timestamps`, …).
    pub experimental_features: BTreeSet<String>,
    /// `builtins.nixVersion`.
    pub nix_version: String,
    /// `eval-cores`: 1 (upstream Nix's default) evaluates sequentially; any
    /// other value enables the parallel evaluator's observable
    /// differences, chiefly that JSON serialisation pre-forces values and
    /// reports errors without serialisation traces. 0, which Determinate
    /// Nix ships with, leaves the number of workers to the evaluator: the
    /// threads of the performance cores
    /// ([`crate::cpus::performance_threads`]).
    pub eval_cores: u32,
    /// The `lint-*` settings: how the parser treats URL literals and
    /// short or absolute path literals.
    pub lints: brix_syntax::Lints,
    /// Print the evaluator's [`Evaluator::stats`] to stderr when it is
    /// dropped (or leaked, see [`Evaluator::leak`]), and a note for every
    /// file a compiled image cannot serve. Front ends set it from
    /// `BRIX_TIMINGS`; the library reads no environment variable.
    pub timings: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            store_dir: StoreDir::default(),
            max_call_depth: 10_000,
            show_trace: true,
            trace_verbose: false,
            allow_import_from_derivation: true,
            experimental_features: BTreeSet::new(),
            nix_version: "2.35.3".to_owned(),
            eval_cores: 1,
            lints: brix_syntax::Lints::default(),
            timings: false,
        }
    }
}

/// Pure mode's allow list (`AllowListSourceAccessor`): the paths readable
/// with everything below them. Each is kept once, in the order it was
/// allowed: the host's first, which most reads fall under, so a check
/// usually stops at the first entry. A path is allowed again every time
/// evaluation makes a string of it (a `toFile` or `builtins.path` called
/// again for the same result), which must not grow the list.
#[derive(Default)]
pub(crate) struct AllowList {
    paths: Vec<CanonPath>,
    known: hashbrown::HashSet<CanonPath>,
}

impl AllowList {
    pub(crate) fn allow(&mut self, path: CanonPath) {
        if !self.known.contains(&path) {
            self.known.insert(path.clone());
            self.paths.push(path);
        }
    }

    /// Whether `path` (absolute) is an allowed path or below one.
    pub(crate) fn allows(&self, path: &[u8]) -> bool {
        self.paths.iter().any(|allowed| {
            let allowed = allowed.as_bytes();
            path == allowed
                || allowed == b"/"
                || path
                    .strip_prefix(allowed)
                    .is_some_and(|rest| rest.first() == Some(&b'/'))
        })
    }
}

/// [`Evaluator::copy_path_to_store`]'s results by path value (accessor and
/// path, before access checks and symlink resolution): the destination
/// and its base name in the arena.
pub(crate) type PathCopies<'a> = hashbrown::HashMap<(AccessorId, BString), (StorePath, &'a str)>;

/// Borrowed lookup avoids allocating a path just to hit the copy cache.
pub(crate) struct PathCopyLookup<'p>(pub AccessorId, pub &'p [u8]);
impl std::hash::Hash for PathCopyLookup<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&(self.0, self.1), state);
    }
}
impl hashbrown::Equivalent<(AccessorId, BString)> for PathCopyLookup<'_> {
    fn equivalent(&self, key: &(AccessorId, BString)) -> bool {
        self.0 == key.0 && self.1 == key.1.as_slice()
    }
}

/// [`Evaluator::eval_file`]'s results by path value (accessor and path as
/// the value holds them, before access checks and resolution): the cell
/// holding the imported file's value. Nix keeps `importResolutionCache`
/// (path to resolved path) and `fileEvalCache` (resolved path to value);
/// this is the two composed, so importing a path value again is one
/// lookup that builds no path.
pub(crate) type Imports<'a> = hashbrown::HashMap<(AccessorId, &'a [u8]), ValueRef<'a>>;

/// Pre-allocated values every evaluation shares.
pub struct Consts<'a> {
    pub true_: ValueRef<'a>,
    pub false_: ValueRef<'a>,
    pub null: ValueRef<'a>,
    pub empty_list: ValueRef<'a>,
    pub empty_attrs: Bindings<'a>,
    pub empty_string: ValueRef<'a>,
    /// The placeholder in environment slots that have not been filled
    /// yet; `maybe_thunk` recognises it by address.
    pub uninit: ValueRef<'a>,
}

pub struct Evaluator<'a> {
    pub(crate) metadata_gate: crate::runtime::Gate,
    /// Runtime allocations. Kept separate from code and immutable literals
    /// so the latter do not acquire untracked pointers into a moving heap.
    pub(crate) bump: &'a Bump,
    pub(crate) code_bump: &'a Bump,
    /// [`Config::timings`], kept here for the hot paths: phases that are
    /// too frequent to time unconditionally (closure building) are timed
    /// only then.
    pub(crate) timings: bool,
    pub config: Config,
    pub(crate) host: &'a dyn Host,
    pub(crate) symbols: RefCell<SymbolTable<'a>>,
    /// The names and comparison keys of all symbols, which every operation
    /// that orders attribute names takes (sets do not carry it).
    pub(crate) keys: &'a SymbolKeys<'a>,
    /// The string value of each symbol's name, indexed by symbol id: made
    /// the first time the name is needed as a string and shared by every
    /// later use (see [`Evaluator::symbol_value`]).
    pub(crate) symbol_values: RefCell<Vec<Option<ValueRef<'a>>>>,
    /// Page table of the evaluator-wide node space: entry `p` is the code
    /// whose nodes occupy page `p` (see [`crate::code::NODE_PAGE_BITS`]).
    /// Read on every force. The arena slice has spare capacity beyond
    /// [`Evaluator::node_pages_len`] and is reallocated at twice the size
    /// when it fills, so linking thousands of files does not copy the
    /// table once per file. Entries past the length are never read.
    pub(crate) node_pages: Cell<&'a [Cell<&'a LinkedCode<'a>>]>,
    /// Entries of [`Evaluator::node_pages`] in use.
    pub(crate) node_pages_len: Cell<usize>,
    /// Where the next linked file's node range starts.
    pub(crate) next_node_base: Cell<u32>,
    pub(crate) infos: RefCell<Vec<Rc<SourceInfo>>>,
    pub(crate) accessors: RefCell<AccessorRegistry>,
    pub(crate) root_accessor: AccessorId,
    pub(crate) store: RefCell<VirtualStore>,
    pub(crate) consts: Consts<'a>,
    pub(crate) call_depth: CallDepth,
    /// Where on the current stack segment [`Evaluator::with_stack`] may
    /// skip asking `stacker` (see [`crate::stack`]).
    pub(crate) stack_window: crate::stack::StackWindowCell,
    /// The top-level environment holding the builtins.
    pub(crate) base_env: Env<'a>,
    pub(crate) base_env_len: Cell<u32>,
    pub(crate) base_static_env: RefCell<StaticEnv<'static>>,
    /// The `builtins` attribute set, filled while builtins are registered.
    pub(crate) builtins_attrs: RefCell<Vec<Attr<'a>>>,
    pub(crate) builtins_cell: ValueRef<'a>,
    pub(crate) file_cache: RefCell<HashMap<SourcePath, ValueRef<'a>>>,
    /// `srcToStore`: the store path each source path was copied to, so a
    /// path interpolated many times is hashed once.
    pub(crate) src_to_store: RefCell<HashMap<SourcePath, StorePath>>,
    pub(crate) path_copies: RefCell<PathCopies<'a>>,
    /// Trees mounted before their store path is known (see [`crate::lazy_tree`]).
    pub(crate) lazy_trees: RefCell<Vec<crate::runtime::rc::Rc<crate::lazy_tree::LazyTree>>>,
    pub(crate) imports: RefCell<Imports<'a>>,
    pub(crate) regex_cache: RefCell<HashMap<Vec<u8>, Rc<crate::regex::Regex>>>,
    /// Paths readable in pure mode besides the store (`allowPath`).
    allowed_paths: RefCell<AllowList>,
    /// Built-ins reachable by name for the evaluator's own use
    /// (`readFileType` for `readDir`, `derivationStrict` for `derivation`).
    pub(crate) internal: RefCell<HashMap<&'static str, ValueRef<'a>>>,
    /// The value of `builtins.derivation`, set after the builtins exist.
    pub(crate) derivation_fn: Cell<Option<ValueRef<'a>>>,
    /// Every store path named so far, with its arena renderings, so that
    /// context elements and `outPath` strings share one copy per path.
    pub(crate) store_paths: RefCell<crate::builtins::derivation::StorePathInterner<'a>>,
    pub(crate) sealed: Cell<bool>,
    /// Values already visited by the JSON pre-force walk (Nix keeps this
    /// set for the whole process).
    pub(crate) deep_seen: RefCell<std::collections::HashSet<usize>>,
    pub(crate) root: std::sync::Arc<crate::mount::RootAccessor>,
    pub(crate) corepkgs: Cell<Option<AccessorId>>,
    pub(crate) stats: crate::stats::Stats,
    /// `pos_bases[i]` is the start of code `i`'s range in the position
    /// space (see [`SrcPos`]); `next_pos_base` is where the next loaded
    /// file's range begins.
    pub(crate) pos_bases: RefCell<Vec<u32>>,
    pub(crate) next_pos_base: Cell<u32>,
    /// Compiled-input images serving mounted trees (see [`crate::image`]).
    pub(crate) images: RefCell<Vec<crate::image::MountedImage>>,
    pub(crate) image_ctx: crate::runtime::cell::OnceCell<crate::image::ImageCompileContext>,
    /// How many symbols existed when construction finished: the ones
    /// every evaluator built like this one shares, which images refer to
    /// by id.
    pub(crate) base_symbol_count: Cell<u32>,
}

impl Drop for Evaluator<'_> {
    fn drop(&mut self) {
        self.report_timings();
    }
}

/// Size of the environment frame holding the builtins (`BASE_ENV_SIZE`
/// is 128 in Nix; brix leaves room for host-supplied extras).
const BASE_ENV_SIZE: usize = 256;

impl<'a> Evaluator<'a> {
    /// Creates an evaluator whose values live in `bump`.
    pub fn new(bump: &'a Bump, config: Config, host: &'a dyn Host) -> Evaluator<'a> {
        Evaluator::with_builtins(bump, config, host, |_| {})
    }

    /// Like [`Evaluator::new`], but lets the caller register additional
    /// built-ins (with [`Evaluator::add_primop`] or
    /// [`Evaluator::add_global`]) before the base scope is sealed. The
    /// flake layer adds `parseFlakeRef`, `flakeRefToString` and `getFlake`
    /// this way.
    pub fn with_builtins(
        bump: &'a Bump,
        config: Config,
        host: &'a dyn Host,
        register: impl FnOnce(&Evaluator<'a>),
    ) -> Evaluator<'a> {
        Self::with_arenas(bump, bump, config, host, register)
    }

    /// Builds an evaluator with separate runtime and permanent-code arenas.
    /// Both arenas must outlive the evaluator and its borrowed values.
    /// This separates allocation domains; it does not enable reclamation.
    pub fn with_arenas(
        bump: &'a Bump,
        code_bump: &'a Bump,
        config: Config,
        host: &'a dyn Host,
        register: impl FnOnce(&Evaluator<'a>),
    ) -> Evaluator<'a> {
        let keys = SymbolKeys::new(code_bump);
        let symbols = SymbolTable::new(keys);
        let mut accessors = AccessorRegistry::new();
        let root = std::sync::Arc::new(crate::mount::RootAccessor::new(host.root_accessor()));
        let root_accessor = accessors.register(root.clone());
        let dummy: ValueRef<'a> = code_bump.alloc(ValueCell::new(Value::Null));
        let base_env = Env::new(bump, None, BASE_ENV_SIZE, |_| dummy);
        let empty_attrs = Bindings::empty(code_bump, SrcPos::NONE);
        let consts = Consts {
            true_: code_bump.alloc(ValueCell::new(Value::Bool(true))),
            false_: code_bump.alloc(ValueCell::new(Value::Bool(false))),
            null: code_bump.alloc(ValueCell::new(Value::Null)),
            empty_list: code_bump.alloc(ValueCell::new(Value::List(
                code_bump.alloc(List { elems: &[] }),
            ))),
            empty_attrs,
            empty_string: code_bump.alloc(ValueCell::new(Value::String(
                code_bump.alloc(NixStr::new(&[], &[])),
            ))),
            uninit: code_bump.alloc(ValueCell::new(Value::Blackhole)),
        };
        let mut allowed_paths = AllowList::default();
        for path in host.allowed_paths() {
            allowed_paths.allow(path);
        }
        let store = RefCell::new(VirtualStore::new(
            config.store_dir.clone(),
            brix_store::XpSettings::from_names(
                config.experimental_features.iter().map(String::as_str),
            ),
        ));
        let ev = Evaluator {
            bump,
            code_bump,
            timings: config.timings,
            config,
            host,
            metadata_gate: Default::default(),
            symbols: RefCell::new(symbols),
            keys,
            symbol_values: RefCell::new(Vec::new()),
            node_pages: Cell::new(&[]),
            node_pages_len: Cell::new(0),
            next_node_base: Cell::new(0),
            infos: RefCell::new(Vec::new()),
            accessors: RefCell::new(accessors),
            root_accessor,
            store,
            consts,
            call_depth: CallDepth::new(),
            stack_window: crate::stack::StackWindowCell::new(),
            base_env,
            base_env_len: Cell::new(0),
            base_static_env: RefCell::new(StaticEnv::default()),
            builtins_attrs: RefCell::new(Vec::new()),
            builtins_cell: bump.alloc(ValueCell::new(Value::Attrs(empty_attrs))),
            file_cache: RefCell::new(HashMap::new()),
            src_to_store: RefCell::new(HashMap::new()),
            path_copies: RefCell::new(hashbrown::HashMap::new()),
            lazy_trees: RefCell::new(Vec::new()),
            imports: RefCell::new(hashbrown::HashMap::new()),
            regex_cache: RefCell::new(HashMap::new()),
            allowed_paths: RefCell::new(allowed_paths),
            internal: RefCell::new(HashMap::new()),
            derivation_fn: Cell::new(None),
            store_paths: RefCell::new(Default::default()),
            sealed: Cell::new(false),
            deep_seen: RefCell::new(std::collections::HashSet::new()),
            root,
            corepkgs: Cell::new(None),
            stats: crate::stats::Stats::default(),
            pos_bases: RefCell::new(Vec::new()),
            next_pos_base: Cell::new(0),
            images: RefCell::new(Vec::new()),
            image_ctx: crate::runtime::cell::OnceCell::new(),
            base_symbol_count: Cell::new(0),
        };
        crate::builtins::register_all(&ev);
        register(&ev);
        crate::builtins::finish(&ev).expect("the built-in derivation wrapper must evaluate");
        ev.base_symbol_count.set(ev.symbols.borrow().len() as u32);
        ev
    }

    /// Counters describing the work done so far.
    pub fn stats(&self) -> &crate::stats::Stats {
        &self.stats
    }

    fn report_timings(&self) {
        if self.timings {
            eprintln!("brix timings:\n{}", self.stats.render());
        }
    }

    /// Ends the evaluator without freeing what it holds outside the arena
    /// (the symbol table, parsed files, the virtual store's derivations),
    /// for a process that exits right after: the operating system takes
    /// it back at exit in one go. [`Config::timings`] are reported as
    /// dropping would. The arena belongs to the caller, who leaks it too or
    /// not.
    ///
    /// Whatever the evaluator shares with its host (the source accessors,
    /// and through them the host's store connection) stays referenced, so
    /// a host that must release a resource has to do so explicitly.
    pub fn leak(self) {
        self.report_timings();
        std::mem::forget(self);
    }

    // ----- allocation helpers -----

    pub fn alloc_cell(&self, v: Value<'a>) -> ValueRef<'a> {
        self.bump.alloc(ValueCell::new(v))
    }

    pub fn alloc_str(&self, s: &[u8]) -> &'a [u8] {
        self.bump.alloc_slice_copy(s)
    }

    pub fn alloc_string_value(&self, bytes: &[u8], context: &[ContextElem<'a>]) -> Value<'a> {
        if bytes.is_empty() && context.is_empty() {
            return self.consts.empty_string.get();
        }
        crate::stats::Stats::bump_work(&self.stats.strings);
        Value::String(self.bump.alloc(NixStr::new(
            self.bump.alloc_slice_copy(bytes),
            self.bump.alloc_slice_copy(context),
        )))
    }

    /// Allocates a string whose bytes are already in the arena.
    pub fn string_from_arena(&self, bytes: &'a [u8], context: &[ContextElem<'a>]) -> Value<'a> {
        Value::String(
            self.bump
                .alloc(NixStr::new(bytes, self.bump.alloc_slice_copy(context))),
        )
    }

    pub fn mk_string(&self, s: &str) -> Value<'a> {
        self.alloc_string_value(s.as_bytes(), &[])
    }

    /// A string from bytes that must be copied into the arena, with the
    /// context accumulated while producing them.
    pub fn mk_string_ctx(&self, s: &[u8], ctx: &StringContext<'a>) -> Value<'a> {
        let context = ctx.finish(self.bump);
        if s.is_empty() && context.is_empty() {
            return self.consts.empty_string.get();
        }
        crate::stats::Stats::bump_work(&self.stats.strings);
        Value::String(
            self.bump
                .alloc(NixStr::new(self.bump.alloc_slice_copy(s), context)),
        )
    }

    /// A string over bytes that already live in the arena (a coerced
    /// string, a substring of one), with an accumulated context. Nothing
    /// but the 32-byte header is allocated; when the context was borrowed
    /// from a single string it is shared too.
    pub fn mk_string_arena(&self, bytes: &'a [u8], ctx: &StringContext<'a>) -> Value<'a> {
        self.mk_string_view(bytes, ctx.finish(self.bump))
    }

    /// A string sharing arena bytes and an arena context slice: how
    /// `substring`, `baseNameOf`, `match` and `split` return parts of an
    /// existing string without copying.
    pub fn mk_string_view(&self, bytes: &'a [u8], context: &'a [ContextElem<'a>]) -> Value<'a> {
        if bytes.is_empty() && context.is_empty() {
            return self.consts.empty_string.get();
        }
        crate::stats::Stats::bump_work(&self.stats.strings);
        Value::String(self.bump.alloc(NixStr::new(bytes, context)))
    }

    /// Copies a store path basename into the arena (for context elements).
    pub fn arena_str(&self, s: &str) -> &'a str {
        self.bump.alloc_str(s)
    }

    /// A path value for `path` (absolute, canonical, bytes) on `accessor`,
    /// its bytes copied into the arena.
    pub fn mk_path(&self, accessor: AccessorId, path: impl AsRef<[u8]>) -> Value<'a> {
        Value::Path(self.bump.alloc(PathVal {
            accessor,
            path: self.bump.alloc_slice_copy(path.as_ref()),
        }))
    }

    pub fn mk_root_path(&self, path: &CanonPath) -> Value<'a> {
        self.mk_path(self.root_accessor, path.as_bytes())
    }

    pub fn mk_list(&self, elems: &[ValueRef<'a>]) -> Value<'a> {
        if elems.is_empty() {
            return self.consts.empty_list.get();
        }
        crate::stats::Stats::bump_work(&self.stats.lists);
        Value::List(self.bump.alloc(List {
            elems: self.bump.alloc_slice_copy(elems),
        }))
    }

    /// A list of `len` elements produced by `elems`, written straight into
    /// the arena.
    pub fn mk_list_from_iter(
        &self,
        len: usize,
        elems: impl ExactSizeIterator<Item = ValueRef<'a>>,
    ) -> Value<'a> {
        if len == 0 {
            return self.consts.empty_list.get();
        }
        crate::stats::Stats::bump_work(&self.stats.lists);
        let slice = self.bump.alloc_slice_fill_iter(elems);
        debug_assert_eq!(slice.len(), len);
        Value::List(self.bump.alloc(List { elems: slice }))
    }

    pub fn mk_list_of_values(&self, values: impl IntoIterator<Item = Value<'a>>) -> Value<'a> {
        let cells: Vec<ValueRef<'a>> = values.into_iter().map(|v| self.alloc_cell(v)).collect();
        self.mk_list(&cells)
    }

    /// Builds a set from attributes in any order; later duplicates win.
    pub fn mk_attrs(&self, mut attrs: Vec<Attr<'a>>, pos: SrcPos) -> Value<'a> {
        if attrs.is_empty() {
            return Value::Attrs(self.consts.empty_attrs);
        }
        Bindings::sort_dedup(&mut attrs);
        self.mk_attrs_sorted(&attrs, pos)
    }

    /// Builds a set of `len` attributes, the `k`-th in canonical order
    /// (unique names in ascending symbol id) being `attr(k)`, written
    /// straight into the set (see [`Bindings::from_fn`]).
    pub fn mk_attrs_from_fn(
        &self,
        len: usize,
        pos: SrcPos,
        attr: impl FnMut(usize) -> Attr<'a>,
    ) -> Value<'a> {
        if len == 0 {
            return Value::Attrs(self.consts.empty_attrs);
        }
        let b = Bindings::from_fn(self.bump, len, pos, attr);
        debug_assert!(b.is_canonical());
        self.mk_bindings(b)
    }

    /// Builds a set from canonically ordered attributes (unique names in
    /// ascending symbol id).
    pub fn mk_attrs_sorted(&self, attrs: &[Attr<'a>], pos: SrcPos) -> Value<'a> {
        self.mk_attrs_from_fn(attrs.len(), pos, |k| attrs[k])
    }

    /// The value of a finished set. Every set the evaluator makes passes
    /// through here, so the empty set is always the shared one.
    pub fn mk_bindings(&self, b: Bindings<'a>) -> Value<'a> {
        if b.is_empty() {
            return Value::Attrs(self.consts.empty_attrs);
        }
        self.count_attrs(b.len());
        Value::Attrs(b)
    }

    fn count_attrs(&self, len: usize) {
        crate::stats::Stats::bump_work(&self.stats.attr_sets);
        crate::stats::Stats::add_work(&self.stats.attrs, len as u64);
        self.stats.attr_set_sizes.record(len);
    }

    pub fn attr(&self, name: Symbol, value: Value<'a>) -> Attr<'a> {
        Attr::new(name, SrcPos::NONE, self.alloc_cell(value))
    }

    pub fn attr_cell(&self, name: Symbol, value: ValueRef<'a>) -> Attr<'a> {
        Attr::new(name, SrcPos::NONE, value)
    }

    pub fn mk_thunk(
        &self,
        code: &'a LinkedCode<'a>,
        node: crate::ir::NodeId,
        env: Env<'a>,
    ) -> ValueRef<'a> {
        crate::stats::Stats::bump_work(&self.stats.thunks);
        self.alloc_cell(Value::Thunk {
            env,
            node: GlobalNode(code.node_base + node.0),
        })
    }

    /// A thunk for an already resolved [`GlobalNode`].
    #[inline]
    pub(crate) fn mk_thunk_global(&self, env: Env<'a>, node: GlobalNode) -> ValueRef<'a> {
        crate::stats::Stats::bump_work(&self.stats.thunks);
        self.alloc_cell(Value::Thunk { env, node })
    }

    /// The file and node a [`GlobalNode`] names: one page-table load and a
    /// subtraction (see `crate::code::NODE_PAGE_BITS`).
    #[inline]
    pub fn resolve_node(&self, node: GlobalNode) -> (&'a LinkedCode<'a>, crate::ir::NodeId) {
        let code = self.node_pages.get()[(node.0 >> crate::code::NODE_PAGE_BITS) as usize].get();
        (code, crate::ir::NodeId(node.0 - code.node_base))
    }

    /// The definition of the function a [`Value::Lambda`] closes over (its
    /// `node`), and the file it is in.
    #[inline]
    pub fn lambda_def(
        &self,
        node: GlobalNode,
    ) -> (&'a LinkedCode<'a>, &'a crate::code::Lambda<'a>) {
        let (code, id) = self.resolve_node(node);
        match *code.node(id) {
            Node::Lambda(l) => (code, code.lambda(l)),
            _ => unreachable!("a lambda value points at a lambda node"),
        }
    }

    /// `Value::isTrivial`: whether forcing `cell` runs no code that could
    /// fail or loop. A value already forced is; so is a thunk of an
    /// attribute set without dynamic attributes, of a list or of a
    /// function, whose evaluation only allocates. Any other thunk, and a
    /// suspended application, is not. `readFlake` forces only what is
    /// trivial (`forceTrivialValue`).
    pub fn is_trivial(&self, cell: ValueRef<'a>) -> bool {
        match cell.get() {
            Value::Thunk { node, .. } => {
                let (code, id) = self.resolve_node(node);
                match *code.node(id) {
                    Node::Attrs(attrs) => code.attr_set(attrs).dynamic_attrs.is_empty(),
                    Node::List { .. } | Node::Lambda(_) => true,
                    _ => false,
                }
            }
            Value::App(_) => false,
            _ => true,
        }
    }

    /// A suspended application `fun arg`.
    pub fn mk_app(&self, fun: ValueRef<'a>, arg: ValueRef<'a>) -> ValueRef<'a> {
        self.alloc_cell(Value::App(self.bump.alloc(App { fun, arg })))
    }

    pub fn mk_primop_app(&self, fun: ValueRef<'a>, arg: ValueRef<'a>) -> Value<'a> {
        Value::PrimOpApp(self.bump.alloc(PrimOpApp { fun, arg }))
    }

    /// A frame of `size` slots, each holding the `uninit` placeholder until
    /// it is filled.
    /// Out of line so that the closures that make frames share one copy.
    #[inline(never)]
    pub fn alloc_env(&self, up: Option<Env<'a>>, size: usize) -> Env<'a> {
        crate::stats::Stats::bump_work(&self.stats.envs);
        let uninit = self.consts.uninit;
        Env::new(self.bump, up, size, |_| uninit)
    }

    /// A frame holding `values`.
    pub fn alloc_env_with(&self, up: Option<Env<'a>>, values: &[ValueRef<'a>]) -> Env<'a> {
        Env::new(self.bump, up, values.len(), |i| values[i])
    }

    pub fn alloc_primop(&self, p: PrimOp<'a>) -> ValueRef<'a> {
        self.alloc_cell(Value::PrimOp(self.code_bump.alloc(p)))
    }

    // ----- symbols -----

    pub fn intern(&self, name: &[u8]) -> Symbol {
        // Nearly every name interned during evaluation exists already. In
        // the parallel build the table is behind a reader-writer lock, and
        // taking it exclusively for each lookup made twenty workers queue.
        #[cfg(brix_parallel)]
        if let Some(symbol) = self.symbols.borrow().get(name) {
            return symbol;
        }
        self.symbols.borrow_mut().intern(name)
    }

    pub fn sym(&self, name: &str) -> Symbol {
        self.intern(name.as_bytes())
    }

    pub fn symbol_name(&self, s: Symbol) -> &'a [u8] {
        self.keys.name(s)
    }

    /// The name of `sym` as a string value (no context), in a cell shared
    /// by every caller: `attrNames`, `mapAttrs`, `filterAttrs` and
    /// `zipAttrsWith` hand names to Nix code millions of times over a
    /// nixpkgs evaluation, and a fresh cell, header and copy of the bytes
    /// each time was a quarter of all strings brix made. The cell is made on
    /// first use; its bytes are the symbol table's own.
    ///
    /// Nix does the same: every symbol is stored as a ready-made string
    /// `Value` (`SymbolValue` in `symbol-table.hh`) and those four primops
    /// put `Value::toPtr(state.symbols[name])` straight into the result.
    ///
    /// Sharing a cell is sound because nothing ever writes to it: the
    /// only writers of a cell are `force` (and `fail_cell`), which write
    /// only cells holding a thunk or a suspended application, and code that
    /// fills a cell it has just allocated itself (an import's result, the
    /// `builtins` set, a primop being registered). Environment slots are
    /// `Cell<ValueRef>` and are re-pointed, never written through. Nor is
    /// sharing observable: `==`'s pointer shortcut only skips comparing two
    /// strings that are equal anyway, the printers' `«repeated»` tracking
    /// looks only at sets and lists, and the deep-force walks treat a string
    /// as a leaf whether or not they have seen its cell before.
    pub fn symbol_value(&self, sym: Symbol) -> ValueRef<'a> {
        let i = sym.0 as usize;
        if let Some(&Some(cell)) = self.symbol_values.borrow().get(i) {
            return cell;
        }
        let cell = self
            .code_bump
            .alloc(ValueCell::new(self.literal_string(self.keys.name(sym))));
        let mut cells = self.symbol_values.borrow_mut();
        if cells.len() <= i {
            cells.resize(self.keys.len().max(i + 1), None);
        }
        cells[i] = Some(cell);
        cell
    }

    pub fn symbol_str(&self, s: Symbol) -> String {
        String::from_utf8_lossy(self.symbol_name(s)).into_owned()
    }

    /// A symbol as Nix prints a `SymbolStr` in messages: through
    /// `printIdentifier`, so names that are not valid identifiers come out
    /// quoted (`attribute '"a b"' missing`).
    pub fn symbol_ident(&self, s: Symbol) -> Vec<u8> {
        let mut out = Vec::new();
        brix_syntax::printer::print_identifier(&mut out, self.symbol_name(s));
        out
    }

    // ----- builtins registration -----

    /// Adds a name to the top-level scope and to `builtins`
    /// (`addConstant`/`addPrimOp`). Only valid before any code is loaded.
    pub fn add_global(&self, name: &str, value: ValueRef<'a>) {
        assert!(
            !self.sealed.get(),
            "globals must be registered before code is loaded"
        );
        let env_name = self.sym(name);
        let builtin_name = self.sym(name.strip_prefix("__").unwrap_or(name));
        let displ = self.base_env_len.get();
        assert!((displ as usize) < BASE_ENV_SIZE, "too many globals");
        self.base_env.set_slot(displ as usize, value);
        self.base_env_len.set(displ + 1);
        self.base_static_env
            .borrow_mut()
            .vars
            .push((env_name, displ));
        self.builtins_attrs
            .borrow_mut()
            .push(Attr::new(builtin_name, SrcPos::NONE, value));
    }

    /// Registers a built-in function.
    pub fn add_primop(
        &self,
        name: &'static str,
        arity: u8,
        imp: &'a crate::value::PrimOpFn<'a>,
    ) -> ValueRef<'a> {
        let display = name.strip_prefix("__").unwrap_or(name);
        let cell = self.alloc_primop(PrimOp {
            name: display,
            arity,
            add_trace: true,
            imp,
        });
        self.add_global(name, cell);
        self.internal.borrow_mut().insert(display, cell);
        cell
    }

    /// Finishes registration: sorts the scope and `builtins`.
    pub(crate) fn seal(&self) {
        let _metadata = self.metadata_gate.lock();
        if self.sealed.replace(true) {
            return;
        }
        let mut senv = self.base_static_env.borrow_mut();
        senv.vars.sort_by_key(|&(s, _)| s);
        let mut attrs = self.builtins_attrs.borrow_mut();
        attrs.sort_by_key(|a| a.name);
        self.builtins_cell
            .set(self.mk_attrs_sorted(&attrs, SrcPos::NONE));
    }

    /// A built-in by its display name, for internal use.
    pub fn builtin(&self, name: &str) -> ValueRef<'a> {
        self.internal
            .borrow()
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("builtin '{name}' not registered"))
    }

    // ----- code loading -----

    /// Compiles and loads an expression. Variables are resolved against
    /// the builtins plus `extra_scope`, if given (for `scopedImport`).
    pub fn load(
        &self,
        expr: &brix_syntax::ast::Expr,
        origin: Origin,
        source: Vec<u8>,
        extra_scope: Option<&StaticEnv<'_>>,
        is_internal: bool,
    ) -> Result<&'a LinkedCode<'a>> {
        let _metadata = self.metadata_gate.lock();
        self.seal();
        let started = crate::platform::Instant::now();
        // Register the source first so that errors found while compiling
        // (undefined variables) have positions in it.
        let (id, pos_base) = self.register_source(origin, source, is_internal);
        let base = self.base_static_env.borrow();
        let code = {
            let mut symbols = self.symbols.borrow_mut();
            match extra_scope {
                Some(scope) => {
                    // The scope sits between the builtins and the file.
                    let env = StaticEnv {
                        is_with: None,
                        up: Some(&base),
                        vars: scope.vars.clone(),
                    };
                    compile::compile(expr, &mut symbols, &env)
                }
                None => compile::compile(expr, &mut symbols, &base),
            }
        };
        drop(base);
        let code = match code {
            Ok(c) => c,
            Err(compile::CompileError::UndefinedVariable { name, pos }) => {
                // Nix reports this from `bindVars`, before evaluating.
                let at = if pos.is_none() {
                    SrcPos::NONE
                } else {
                    SrcPos(pos_base + pos.0)
                };
                return Err(self.error_at(
                    ErrorKind::UndefinedVar,
                    compile::undefined_variable(&name),
                    at,
                ));
            }
        };
        let linked = self.link(id, pos_base, code);
        crate::stats::Stats::add_time(&self.stats.compile_time, started.elapsed());
        Ok(linked)
    }

    /// Gives a source its id and its range in the position space, and
    /// keeps its text for messages. Every parsed file goes through here
    /// (or [`Self::register_source_lazy`]), whether or not it then compiles,
    /// so a [`CodeId`] counts registered sources, not linked ones.
    /// `is_internal` marks the evaluator's own helpers
    /// ([`Evaluator::is_internal_pos`]).
    pub(crate) fn register_source(
        &self,
        origin: Origin,
        source: Vec<u8>,
        is_internal: bool,
    ) -> (CodeId, u32) {
        let _metadata = self.metadata_gate.lock();
        let mut infos = self.infos.borrow_mut();
        let id = CodeId(infos.len() as u32);
        // The file's range in the position space: one more than its
        // length so that an offset equal to the length (end of file) is
        // still inside it. Nix's `PosTable` has the same 4 GiB limit on
        // total source size.
        let base = self.next_pos_base.get();
        let span = u32::try_from(source.len() + 1).expect("source file too large");
        let next = base
            .checked_add(span)
            .expect("more than 4 GiB of source loaded in one evaluator");
        self.next_pos_base.set(next);
        self.pos_bases.borrow_mut().push(base);
        let mut info = SourceInfo::new(origin, source);
        info.is_internal = is_internal;
        infos.push(Rc::new(info));
        (id, base)
    }

    /// Like [`Self::register_source`] for a file whose text is fetched only
    /// when an excerpt needs it (a module served from a compiled image):
    /// the length is known, the line table comes from `lines` and the
    /// text from `loader` (see [`SourceInfo::lazy`]).
    pub(crate) fn register_source_lazy(
        &self,
        origin: Origin,
        len: u32,
        lines: Box<crate::runtime::Loader<u32>>,
        loader: Box<crate::runtime::Loader<u8>>,
    ) -> (CodeId, u32) {
        let _metadata = self.metadata_gate.lock();
        let mut infos = self.infos.borrow_mut();
        let id = CodeId(infos.len() as u32);
        let base = self.next_pos_base.get();
        let span = len.checked_add(1).expect("source file too large");
        let next = base
            .checked_add(span)
            .expect("more than 4 GiB of source loaded in one evaluator");
        self.next_pos_base.set(next);
        self.pos_bases.borrow_mut().push(base);
        infos.push(Rc::new(SourceInfo::lazy(origin, len, lines, loader)));
        (id, base)
    }

    /// Immutable literals captured by compiled closures must stay entirely
    /// in permanent storage, including their payload bytes.
    fn literal_string(&self, bytes: &'a [u8]) -> Value<'a> {
        if bytes.is_empty() {
            return self.consts.empty_string.get();
        }
        crate::stats::Stats::bump_work(&self.stats.strings);
        Value::String(self.code_bump.alloc(NixStr::new(bytes, &[])))
    }

    fn literal_path(&self, bytes: &'a [u8]) -> Value<'a> {
        Value::Path(self.code_bump.alloc(PathVal {
            accessor: self.root_accessor,
            path: bytes,
        }))
    }

    pub(crate) fn link(&self, id: CodeId, base: u32, code: Code) -> &'a LinkedCode<'a> {
        let _metadata = self.metadata_gate.lock();
        let consts: Vec<Option<ValueRef<'a>>> = code
            .nodes
            .iter()
            .map(|n| {
                let value = match *n {
                    Node::Int(i) => Value::Int(i),
                    Node::Float(f) => Value::Float(f),
                    Node::Str(lit) => {
                        self.literal_string(self.code_bump.alloc_slice_copy(code.string(lit)))
                    }
                    Node::Path { lit, .. } => {
                        self.literal_path(self.code_bump.alloc_slice_copy(code.string(lit)))
                    }
                    _ => return None,
                };
                Some(&*self.code_bump.alloc(ValueCell::new(value)))
            })
            .collect();
        let consts = self.code_bump.alloc_slice_copy(&consts);
        self.install_code(code.nodes.len(), |node_base| {
            LinkedCode::link(self.code_bump, id, base, node_base, &code, consts)
        })
    }

    /// Links a module whose tables a compiled image already put in the
    /// arena (see [`crate::image::codec`]): only the literal cells are
    /// left to make. A string literal's value shares the table's bytes,
    /// and so does a path literal's: decoding already wrote its absolute
    /// path (the mount point and the literal) into the arena.
    pub(crate) fn link_image_module(
        &self,
        id: CodeId,
        base: u32,
        tables: crate::image::codec::Linked<'a>,
    ) -> std::result::Result<&'a LinkedCode<'a>, crate::image::ImageError> {
        let _metadata = self.metadata_gate.lock();
        let strings = tables.strings;
        let consts = self
            .code_bump
            .alloc_slice_fill_with(tables.nodes.len(), |i| {
                let value = match tables.nodes[i] {
                    Node::Int(i) => Value::Int(i),
                    Node::Float(f) => Value::Float(f),
                    Node::Str(lit) => self.literal_string(strings[lit.0 as usize]),
                    Node::Path { lit, .. } => self.literal_path(strings[lit.0 as usize]),
                    _ => return None,
                };
                Some(&*self.code_bump.alloc(ValueCell::new(value)))
            });
        Ok(self.install_code(tables.nodes.len(), |node_base| {
            self.code_bump.alloc(LinkedCode {
                id,
                node_base,
                base,
                root: tables.root,
                nodes: tables.nodes,
                consts,
                strings,
                attr_sets: tables.attr_sets,
                lambdas: tables.lambdas,
                attr_names: tables.attr_names,
                node_lists: tables.node_lists,
                parts: tables.parts,
                closures: crate::closure::ClosureTable::new(self.code_bump, tables.nodes.len()),
            })
        }))
    }

    /// Gives a file of `n_nodes` nodes a page-aligned range of the node
    /// space, builds it with that range's start (`make`), and registers
    /// it.
    fn install_code(
        &self,
        n_nodes: usize,
        make: impl FnOnce(u32) -> &'a LinkedCode<'a>,
    ) -> &'a LinkedCode<'a> {
        let page = 1u32 << crate::code::NODE_PAGE_BITS;
        let node_base = self.next_node_base.get();
        let pages = u32::try_from(n_nodes)
            .expect("more than 4 billion nodes in one file")
            .div_ceil(page)
            .max(1);
        let next = node_base
            .checked_add(pages * page)
            .expect("more than 4 billion nodes loaded in one evaluator");
        self.next_node_base.set(next);
        let linked = make(node_base);
        self.add_node_pages(linked, pages as usize);
        linked
    }

    /// Maps the next `pages` pages of the node space to `linked`, growing
    /// the page table geometrically.
    fn add_node_pages(&self, linked: &'a LinkedCode<'a>, pages: usize) {
        let len = self.node_pages_len.get();
        let needed = len + pages;
        let mut table = self.node_pages.get();
        if needed > table.len() {
            let old = table;
            let capacity = needed.max(old.len() * 2).max(64);
            // Unused entries are never read; `linked` is only a filler.
            table = self.code_bump.alloc_slice_fill_with(capacity, |i| {
                Cell::new(if i < len { old[i].get() } else { linked })
            });
            self.node_pages.set(table);
        }
        for entry in &table[len..needed] {
            entry.set(linked);
        }
        self.node_pages_len.set(needed);
    }

    /// The code a position belongs to and the byte offset within it.
    pub(crate) fn locate(&self, pos: SrcPos) -> Option<(CodeId, u32)> {
        if pos.is_none() {
            return None;
        }
        let bases = self.pos_bases.borrow();
        // The last base not greater than the position.
        let idx = bases.partition_point(|&b| b <= pos.0).checked_sub(1)?;
        Some((CodeId(idx as u32), pos.0 - bases[idx]))
    }

    /// Resolves a run-time position for messages.
    pub fn position(&self, pos: SrcPos) -> Option<Position> {
        let (code, offset) = self.locate(pos)?;
        let info = self.infos.borrow().get(code.0 as usize)?.clone();
        let (line, column) = info.line_col(offset);
        Some(Position { info, line, column })
    }

    #[inline]
    pub fn src_pos(&self, code: &LinkedCode<'a>, pos: Pos) -> SrcPos {
        if pos.is_none() {
            SrcPos::NONE
        } else {
            SrcPos(code.base + pos.0)
        }
    }

    /// Whether `pos` is in one of the evaluator's own helper files, whose
    /// positions Nix leaves out of some traces (`isDerivationInternalPos`).
    pub fn is_internal_pos(&self, pos: SrcPos) -> bool {
        self.locate(pos)
            .is_some_and(|(code, _)| self.infos.borrow()[code.0 as usize].is_internal)
    }

    // ----- errors -----

    pub fn error(&self, kind: ErrorKind, msg: impl Into<bstr::BString>) -> crate::Error {
        EvalError::new(kind, msg)
    }

    pub fn error_at(
        &self,
        kind: ErrorKind,
        msg: impl Into<bstr::BString>,
        pos: SrcPos,
    ) -> crate::Error {
        EvalError::new(kind, msg).at(self.position(pos))
    }

    pub fn add_trace(&self, e: &EvalError, pos: SrcPos, hint: impl Into<bstr::BString>) {
        e.add_trace(self.position(pos), hint);
    }

    /// `addCallDepth`: fails with Nix's stack-overflow error past the
    /// configured depth. Returns a guard that decrements on drop.
    pub fn enter(&self, pos: SrcPos) -> Result<DepthGuard<'_, 'a>> {
        if self.call_depth.get() > self.config.max_call_depth {
            return Err(self.error_at(
                ErrorKind::StackOverflow,
                "stack overflow; max-call-depth exceeded",
                pos,
            ));
        }
        self.call_depth.set(self.call_depth.get() + 1);
        Ok(DepthGuard(self))
    }

    // ----- accessors and paths -----

    pub fn accessor(&self, id: AccessorId) -> std::sync::Arc<dyn SourceAccessor> {
        self.accessors.borrow().get(id).clone()
    }

    pub fn register_accessor(&self, accessor: std::sync::Arc<dyn SourceAccessor>) -> AccessorId {
        self.accessors.borrow_mut().register(accessor)
    }

    pub fn source_path(&self, p: &PathVal<'_>) -> SourcePath {
        SourcePath::new(p.accessor, Self::canon_of(p))
    }

    /// Shows a path as Nix does: the accessor's display prefix plus the
    /// absolute path, as bytes (a message quotes them as they are).
    pub fn show_path(&self, p: &PathVal<'_>) -> BString {
        self.accessor(p.accessor).show_path(&Self::canon_of(p))
    }

    /// A path value's path as a [`CanonPath`]. Every path value that
    /// comes from a string went through `CanonPath` (`canon_path`), which
    /// rejects NUL bytes, and path literals and file names cannot hold
    /// any, so this cannot fail.
    fn canon_of(p: &PathVal<'_>) -> CanonPath {
        CanonPath::new(p.path).expect("a path value holds no NUL bytes")
    }

    pub fn store_dir(&self) -> &StoreDir {
        &self.config.store_dir
    }

    /// The store path `path` lies in (`isInStore` and
    /// `toStorePath(path).first`), if any. A path below the store
    /// directory that names no valid store path is `None`: Nix's callers
    /// catch that error and go on as for a path outside the store.
    pub fn maybe_store_path(&self, path: impl AsRef<[u8]>) -> Option<StorePath> {
        self.config.store_dir.store_path_of(path).ok()
    }

    pub fn allow_path(&self, path: CanonPath) {
        self.allowed_paths.borrow_mut().allow(path);
    }

    /// Pure-mode access control (`AllowListSourceAccessor`): only the store
    /// directory and explicitly allowed paths are readable.
    pub fn check_path_access(&self, p: &PathVal<'_>) -> Result<()> {
        if p.accessor != self.root_accessor {
            return Ok(());
        }
        // Nix's pure mode starts with an empty allow list: even valid store
        // paths are readable only once this evaluation added, fetched or
        // mounted them (`allowPath`), or the host allowed them up front.
        let path = p.path;
        if self.allowed_paths.borrow().allows(path) {
            return Ok(());
        }
        Err(self.error(
            ErrorKind::RestrictedPath,
            msg!(
                "access to absolute path '",
                path,
                "' is forbidden in pure evaluation mode (use '--impure' to override)"
            ),
        ))
    }

    pub fn experimental(&self, feature: &str) -> bool {
        self.config.experimental_features.contains(feature)
    }

    pub fn require_experimental(&self, feature: &str, what: &str) -> Result<()> {
        if self.experimental(feature) {
            Ok(())
        } else {
            Err(self.error(
                ErrorKind::Eval,
                // `MissingExperimentalFeature`: the reason sits right after
                // "disabled".
                format!(
                    "experimental Nix feature '{feature}' is disabled{}; add '--extra-experimental-features {feature}' to enable it",
                    if what.is_empty() { String::new() } else { format!(" ({what})") }
                ),
            ))
        }
    }

    /// The virtual store brix "wrote" to during evaluation.
    pub fn virtual_store(&self) -> crate::runtime::cell::Ref<'_, VirtualStore> {
        self.store.borrow()
    }
}

crate::runtime::thread_cell!(CallDepth, u32, 0);

/// Decrements the call depth when dropped.
pub struct DepthGuard<'e, 'a>(&'e Evaluator<'a>);

impl Drop for DepthGuard<'_, '_> {
    fn drop(&mut self) {
        self.0.call_depth.set(self.0.call_depth.get() - 1);
    }
}

/// `<nix/fetchurl.nix>`: the derivation behind `builtins.fetchurl`-style
/// downloads performed by the builder (`builtin:fetchurl`). Written from
/// the documented interface of Nix's `fetchurl.nix`.
pub const COREPKGS_FETCHURL: &str = r#"{ system ? ""
, url
, hash ? ""
, md5 ? ""
, sha1 ? ""
, sha256 ? ""
, sha512 ? ""
, outputHash ?
    if hash != "" then hash
    else if sha512 != "" then sha512
    else if sha1 != "" then sha1
    else if md5 != "" then md5
    else sha256
, outputHashAlgo ?
    if hash != "" then ""
    else if sha512 != "" then "sha512"
    else if sha1 != "" then "sha1"
    else if md5 != "" then "md5"
    else "sha256"
, executable ? false
, unpack ? false
, name ? baseNameOf (toString url)
, impure ? false
}:

derivation ({
  builder = "builtin:fetchurl";
  outputHashMode = if unpack || executable then "recursive" else "flat";
  inherit name url executable unpack;
  system = "builtin";
  preferLocalBuild = true;
  impureEnvVars = [ "http_proxy" "https_proxy" "ftp_proxy" "all_proxy" "no_proxy" ];
  urls = [ url ];
} // (if impure then { __impure = true; } else { inherit outputHashAlgo outputHash; }))
"#;

impl<'a> Evaluator<'a> {
    /// Mounts a tree at an absolute path on the root file system.
    pub fn mount(&self, at: CanonPath, accessor: std::sync::Arc<dyn SourceAccessor>) {
        self.root.mount(at, accessor);
    }

    /// The accessor holding `<nix/…>` files, created on first use.
    pub fn corepkgs_accessor(&self) -> AccessorId {
        let _metadata = self.metadata_gate.lock();
        if let Some(id) = self.corepkgs.get() {
            return id;
        }
        let mut mem = brix_store::MemoryAccessor::new();
        mem.add_file(
            &CanonPath::new("/fetchurl.nix").expect("static path"),
            COREPKGS_FETCHURL.as_bytes().to_vec(),
            false,
        )
        .expect("a file can be added at the root of an empty tree");
        let id = self.register_accessor(std::sync::Arc::new(crate::mount::CorepkgsAccessor::new(
            mem,
        )));
        self.corepkgs.set(Some(id));
        id
    }
}

impl<'a> Evaluator<'a> {
    /// Mounts a tree a host fetched outside of `builtins.fetchTree` (a
    /// flake being locked, for instance) exactly as `fetchTree` would:
    /// at its store path, shown as `«locked url»/…`, recorded in the
    /// virtual store under its NAR hash and readable from now on.
    pub fn mount_fetched_tree(&self, tree: &crate::host::FetchedTree) -> Result<StorePath> {
        crate::builtins::fetch::mount_tree(self, tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_literals_and_runtime_strings_use_separate_arenas() {
        let runtime = Bump::new();
        let permanent = Bump::new();
        let ev = Evaluator::with_arenas(
            &runtime,
            &permanent,
            Config::default(),
            &crate::NullHost,
            |_| {},
        );
        let before_runtime = runtime.allocated_bytes();
        let mut source = vec![b'x'; 1024 * 1024];
        source.insert(0, b'"');
        source.push(b'"');
        let Value::String(literal) = ev.eval_str(&source, "/").unwrap() else {
            panic!("a string literal must evaluate to a string");
        };
        assert_eq!(literal.raw_bytes(), &source[1..source.len() - 1]);
        assert_eq!(runtime.allocated_bytes(), before_runtime);

        let before_permanent = permanent.allocated_bytes();
        let Value::String(dynamic) = ev.alloc_string_value(&source, &[]) else {
            panic!("a runtime string must be a string");
        };
        assert_eq!(dynamic.raw_bytes(), source);
        assert!(runtime.allocated_bytes() > before_runtime);
        assert_eq!(permanent.allocated_bytes(), before_permanent);
    }

    #[test]
    fn the_allow_list_keeps_each_path_once_and_allows_what_is_below() {
        let p = |s: &str| CanonPath::new(s).unwrap();
        let mut list = AllowList::default();
        for _ in 0..3 {
            list.allow(p("/nix/store/aaa-x"));
        }
        list.allow(p("/src"));
        assert_eq!(list.paths.len(), 2);
        assert!(list.allows(b"/src"));
        assert!(list.allows(b"/src/a/b"));
        assert!(!list.allows(b"/srcx"));
        assert!(!list.allows(b"/"));
        assert!(list.allows(b"/nix/store/aaa-x/f"));
        list.allow(CanonPath::root());
        assert!(list.allows(b"/anything"));
    }

    /// Allowed paths and the paths checked against them are bytes: a name
    /// that is not UTF-8 is allowed below an allowed directory, and one
    /// allowed directory's name is not a prefix of another's.
    #[test]
    fn the_allow_list_compares_bytes() {
        let mut list = AllowList::default();
        list.allow(CanonPath::new(b"/d\xe9").unwrap());
        assert!(list.allows(b"/d\xe9"));
        assert!(list.allows(b"/d\xe9/caf\xe9"));
        assert!(!list.allows(b"/d\xe9\xff"));
        assert!(!list.allows(b"/d"));
    }
}
