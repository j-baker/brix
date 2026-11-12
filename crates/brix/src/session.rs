//! The session: one evaluator, its host, and everything evaluated so far.

use std::cell::RefCell;
use std::collections::BTreeSet;

use brix_eval::managed_api::{GcEvaluator, GcValue};
use brix_eval::value::SrcPos;
use brix_eval::{Arena, Evaluator, OwnedValue, PrintOptions};
use brix_flake::{FlakeRef, LockFlags, LockedFlake, ParseOptions};
use brix_store::{Derivation, StoreObject, StorePath};

use crate::config::{Config, Teardown};
use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::flake::{self, TreeHandle};
use crate::host::NativeHost;
use crate::value_slots::Lease;

/// What the evaluator and its arena are built from; lives as long as the
/// session and is never moved.
struct Owner {
    arena: SessionArena,
    code_arena: SessionArena,
    host: NativeHost,
    eval_config: brix_eval::Config,
    gc_config: brix_eval::managed_api::GcConfig,
}

/// The arena for permanent compiler and host metadata: freed with the session or,
/// with [`Teardown::Leak`], allocated for the rest of the process.
enum SessionArena {
    Owned(Arena),
    Leaked(&'static Arena),
}

impl SessionArena {
    fn new(teardown: Teardown) -> SessionArena {
        match teardown {
            Teardown::Free => SessionArena::Owned(Arena::new()),
            Teardown::Leak => SessionArena::Leaked(Box::leak(Box::new(Arena::new()))),
        }
    }

    fn get(&self) -> &Arena {
        match self {
            SessionArena::Owned(arena) => arena,
            SessionArena::Leaked(arena) => arena,
        }
    }

    fn occupied_bytes(&self) -> usize {
        // SAFETY: inspect chunk lengths only, without reading their payloads.
        // This loop neither allocates in nor resets the arena; Arena is !Sync.
        unsafe {
            self.get()
                .iter_allocated_chunks_raw()
                .map(|(_, len)| len)
                .sum()
        }
    }
}

/// The managed evaluator; cursor handles own their root registrations.
struct Inner<'a> {
    /// Present for the session's whole life; taken out only when a
    /// [`Teardown::Leak`] session is dropped, to be leaked.
    ev: Option<RefCell<GcEvaluator<'a>>>,
}

self_cell::self_cell!(
    struct Cell {
        owner: Owner,
        #[not_covariant]
        dependent: Inner,
    }
);

/// What a closure running inside the session sees: the evaluator, the
/// host, and the managed heap accessed by [`Cursor`] handles.
pub(crate) struct Ctx<'e, 'a> {
    pub ev: &'e mut GcEvaluator<'a>,
    pub host: &'e NativeHost,
}

impl Ctx<'_, '_> {
    pub fn keep_value(&self, value: GcValue) -> Lease {
        value
    }
    pub fn slot(&self, value: &Lease) -> GcValue {
        value.clone()
    }
}

/// How to render a value (`nix eval`'s output modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Nix's value syntax, as `nix eval` prints it.
    Nix,
    /// `nix eval --json`.
    Json,
    /// `nix eval --raw`: the value coerced to a string, no newline.
    Raw,
}

/// An argument passed to a top-level function (`--arg`, `--argstr`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoArg {
    /// `--arg name expr`: evaluated lazily, relative to the current directory.
    Expr(String),
    /// `--argstr name string`, `--arg-from-file`, `--arg-from-stdin`.
    String(String),
}

/// An evaluation session. Reachable runtime values live in its managed heap;
/// what callers get back are owned snapshots ([`Value`]), rendered
/// text, or [`Cursor`]s that stay valid as long as the session does.
pub struct Session {
    cell: Cell,
    pub(crate) config: Config,
}

impl Drop for Session {
    /// Ends the session as its [`Teardown`] says. The store connection is
    /// closed here, explicitly, whatever the teardown: the evaluator
    /// reaches it too (through the store accessor), so it must not wait
    /// for the evaluator to be dropped, which [`Teardown::Leak`] never
    /// does. That mode then leaks the evaluator (its arena was leaked
    /// when the session was made); the host is dropped as usual.
    /// Background fetches no thread has started are abandoned first, so
    /// that none starts on a closed store.
    fn drop(&mut self) {
        let teardown = self.config.teardown;
        self.cell.with_dependent_mut(|owner, inner| {
            owner.host.cancel_prefetches();
            owner.host.images().finish_compiling();
            owner.host.close_store();
            match teardown {
                Teardown::Free => {}
                Teardown::Leak => {
                    let evaluator = inner
                        .ev
                        .take()
                        .expect("the evaluator lives as long as the session");
                    if owner.eval_config.timings {
                        eprintln!(
                            "brix timings:\n{}",
                            evaluator.borrow().services().stats().render()
                        );
                        let evaluator = evaluator.borrow();
                        let gc = evaluator.gc_stats();
                        eprintln!(
                            "gc: {} collections, {:.3}s collecting, {} bytes copied, {} bytes reclaimed, {} bytes live, {} bytes peak collection capacity",
                            gc.collections,
                            gc.collection_time.as_secs_f64(),
                            gc.copied_bytes,
                            gc.reclaimed_bytes,
                            gc.last_live_bytes,
                            gc.peak_collection_capacity,
                        );
                        eprintln!(
                            "gc generations: {} minor ({:.3}s), {} major ({:.3}s), {} bytes promoted, {} bytes compacted, {} old bytes reclaimed",
                            gc.minor_collections, gc.minor_collection_time.as_secs_f64(),
                            gc.major_collections, gc.major_collection_time.as_secs_f64(),
                            gc.promoted_bytes, gc.compacted_bytes, gc.old_reclaimed_bytes,
                        );
                        let spaces = evaluator.heap_space_usage();
                        let heap_capacity: usize =
                            spaces.iter().map(|(_, _, capacity)| capacity).sum();
                        eprintln!(
                            "memory: {} bytes heap occupied, {} bytes heap capacity, {} bytes scoped root slots reserved, {} bytes metadata arena occupied ({} capacity), {} bytes code arena occupied ({} capacity)",
                            evaluator.heap_bytes(),
                            heap_capacity,
                            evaluator.root_slot_bytes(),
                            owner.arena.occupied_bytes(),
                            owner.arena.get().allocated_bytes(),
                            owner.code_arena.occupied_bytes(),
                            owner.code_arena.get().allocated_bytes(),
                        );
                        for (name, used, capacity) in spaces {
                            eprintln!(
                                "  heap {name}: {used} bytes occupied, {capacity} bytes capacity"
                            );
                        }
                    }
                    std::mem::forget(evaluator);
                }
            }
        });
    }
}

/// A flake the session has locked and called.
pub struct Flake<'s> {
    session: &'s Session,
    slot: Lease,
    locked: LockedFlake<TreeHandle>,
    /// The `{ outputs; inventory; }` schema value, once computed.
    schema_slot: RefCell<Option<Lease>>,
}

/// An owned, deeply forced value.
pub type Value = OwnedValue;

impl Session {
    /// Builds the host and the evaluator. The store (daemon) is connected
    /// on first use.
    pub fn new(config: Config) -> Result<Session> {
        let host = NativeHost::new(&config)?;
        let owner = Owner {
            arena: SessionArena::new(config.teardown),
            code_arena: SessionArena::new(config.teardown),
            host,
            eval_config: config.eval.clone(),
            gc_config: config.gc,
        };
        let cell = Cell::try_new(owner, |owner| {
            let arena = owner.arena.get();
            let code_arena = owner.code_arena.get();
            let ev = Evaluator::with_arenas(
                arena,
                code_arena,
                owner.eval_config.clone(),
                &owner.host,
                |ev| crate::callflake::register(ev, code_arena, &owner.host),
            );
            let mut ev = GcEvaluator::new(ev, owner.gc_config)
                .map_err(|e| owner.host.eval_error(&e, owner.eval_config.show_trace))?;
            crate::callflake::register_callbacks(&mut ev, &owner.host)
                .map_err(|e| owner.host.eval_error(&e, owner.eval_config.show_trace))?;
            Ok::<_, Error>(Inner {
                ev: Some(RefCell::new(ev)),
            })
        })?;
        let session = Session { cell, config };
        // The inputs the session downloads are compiled as they arrive,
        // for the evaluator it now has.
        session.with(|ctx| {
            ctx.host
                .images()
                .set_compile_context(ctx.ev.services().image_compile_context())
        });
        Ok(session)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Collect unreachable runtime values now. Allocation pressure also
    /// triggers collection automatically during evaluation.
    pub fn collect_garbage(&self) -> Result<()> {
        self.with(|ctx| ctx.ev.collect())
            .map_err(|e| self.eval_error(e))
    }

    /// Collection counters, excluding permanent code and host metadata.
    pub fn gc_stats(&self) -> brix_eval::managed_api::GcStats {
        self.with(|ctx| ctx.ev.gc_stats().clone())
    }

    /// Bytes currently occupied by runtime objects, including garbage until
    /// the next collection. This does not include code or host caches.
    pub fn heap_bytes(&self) -> usize {
        self.with(|ctx| ctx.ev.heap_bytes())
    }

    pub(crate) fn with<R>(&self, f: impl for<'a> FnOnce(&mut Ctx<'_, 'a>) -> R) -> R {
        self.cell.with_dependent(|owner, inner| {
            let mut ev = inner
                .ev
                .as_ref()
                .expect("the evaluator lives as long as the session")
                .borrow_mut();
            f(&mut Ctx {
                ev: &mut ev,
                host: &owner.host,
            })
        })
    }

    pub(crate) fn with_slot<R>(
        &self,
        slot: &Lease,
        f: impl for<'a> FnOnce(&mut Ctx<'_, 'a>, GcValue) -> R,
    ) -> R {
        self.with(|ctx| {
            let cell = ctx.slot(slot);
            f(ctx, cell)
        })
    }

    pub(crate) fn with_slots<R>(
        &self,
        a: &Lease,
        b: &Lease,
        f: impl for<'a> FnOnce(&mut Ctx<'_, 'a>, GcValue, GcValue) -> R,
    ) -> R {
        self.with(|ctx| {
            let ca = ctx.slot(a);
            let cb = ctx.slot(b);
            f(ctx, ca, cb)
        })
    }

    pub(crate) fn eval_error(&self, e: brix_eval::Error) -> Error {
        self.cell
            .borrow_owner()
            .host
            .eval_error(&e, self.config.eval.show_trace)
    }

    /// An evaluation error `msg` under the trace `trace`, rendered as one
    /// the evaluator raised (`error.addTrace(noPos, trace)`). Bytes: it can
    /// quote a value.
    pub fn error_with_trace(&self, msg: impl Into<bstr::BString>, trace: &str) -> Error {
        self.traced_error(msg, SrcPos::NONE, trace)
    }

    /// [`Session::error_with_trace`] with the trace at `pos`.
    pub(crate) fn traced_error(
        &self,
        msg: impl Into<bstr::BString>,
        pos: SrcPos,
        trace: &str,
    ) -> Error {
        let e = brix_eval::EvalError::new(brix_eval::ErrorKind::Eval, msg);
        self.with(|ctx| ctx.ev.services().add_trace(&e, pos, trace));
        self.eval_error(e)
    }

    /// The experimental features the store primitives check, from the
    /// evaluation settings.
    pub(crate) fn xp_settings(&self) -> brix_store::XpSettings {
        brix_store::XpSettings::from_names(
            self.config
                .eval
                .experimental_features
                .iter()
                .map(String::as_str),
        )
    }

    /// Evaluates an expression (`nix eval --expr`). Relative paths resolve
    /// against `base_dir`, which must be absolute. Both are bytes, as in
    /// Nix: the expression's strings and paths, and the directory's name,
    /// need not be UTF-8.
    pub fn eval_expr(&self, expr: impl AsRef<[u8]>, base_dir: impl AsRef<[u8]>) -> Result<Value> {
        self.eval_expr_cursor(expr, base_dir)?.to_owned()
    }

    /// Evaluates an expression and renders it.
    pub fn eval_expr_to_string(
        &self,
        expr: impl AsRef<[u8]>,
        base_dir: impl AsRef<[u8]>,
        format: Format,
    ) -> Result<String> {
        self.eval_expr_cursor(expr, base_dir)?.render(format)
    }

    /// Evaluates an expression lazily; the cursor gives access to it.
    pub fn eval_expr_cursor(
        &self,
        expr: impl AsRef<[u8]>,
        base_dir: impl AsRef<[u8]>,
    ) -> Result<Cursor<'_>> {
        let (expr, base_dir) = (expr.as_ref(), base_dir.as_ref());
        let slot = self.with(|ctx| {
            let v = ctx
                .ev
                .eval_str(expr, base_dir)
                .map_err(|e| self.eval_error(e))?;
            Ok::<_, Error>(ctx.keep_value(v))
        })?;
        Ok(Cursor::new(self, slot, Vec::new()))
    }

    /// Evaluates a Nix file (`nix eval --file`), given by its absolute
    /// path (bytes, as Nix's paths are). The file's directory must be
    /// readable by the host (see [`Config::allowed_paths`]).
    pub fn eval_file_cursor(&self, path: impl AsRef<[u8]>) -> Result<Cursor<'_>> {
        let path = path.as_ref();
        let slot = self.with(|ctx| {
            let canon = brix_store::CanonPath::new(path)?;
            let path = brix_store::SourcePath::new(ctx.ev.root_accessor(), canon);
            let res = ctx
                .ev
                .eval_main_file(&path)
                .map_err(|e| self.eval_error(e))?;
            Ok::<_, Error>(ctx.keep_value(res))
        })?;
        Ok(Cursor::new(self, slot, Vec::new()))
    }

    /// `autoCallFunction`: if the value is a function, calls it with the
    /// matching `args`; otherwise returns it unchanged. `--arg`
    /// expressions are evaluated relative to `base_dir`.
    pub fn auto_call(
        &self,
        value: &Cursor<'_>,
        args: &[(String, AutoArg)],
        base_dir: impl AsRef<[u8]>,
    ) -> Result<Cursor<'_>> {
        if !std::ptr::eq(self, value.session()) {
            return Err(Error::Usage(
                "a cursor belongs to a different session".into(),
            ));
        }
        let base_dir = base_dir.as_ref();
        let slot = self.with_slot(&value.slot, |ctx, cell| {
            let ev = &mut *ctx.ev;
            let mut attrs = Vec::with_capacity(args.len());
            for (name, arg) in args {
                let value = match arg {
                    AutoArg::String(s) => ev.string(s.as_bytes()),
                    AutoArg::Expr(e) => ev.eval_str(e.as_bytes(), base_dir),
                }
                .map_err(|e| self.eval_error(e))?;
                attrs.push((name.as_bytes(), value));
            }
            let bindings = ev.attrs(&attrs).map_err(|e| self.eval_error(e))?;
            let res = ev
                .auto_call(&cell, &bindings)
                .map_err(|e| self.eval_error(e))?;
            Ok::<_, Error>(ctx.keep_value(res))
        })?;
        Ok(Cursor::new(self, slot, value.attr_path.clone()))
    }

    pub(crate) fn render(
        &self,
        ev: &mut GcEvaluator<'_>,
        cell: &GcValue,
        format: Format,
    ) -> Result<Vec<u8>> {
        ev.force(cell).map_err(|e| self.eval_error(e))?;
        let result = match format {
            Format::Nix => ev
                .print_value(cell, &PrintOptions::for_eval())
                .map(Vec::from),
            Format::Json => ev.to_json(cell),
            Format::Raw => ev
                .coerce_with_context(
                    cell,
                    brix_eval::SrcPos::NONE,
                    brix_eval::Coercion::COPY,
                    "while generating the eval command output",
                )
                .and_then(|s| ev.string_parts(&s).map(|(bytes, _)| bytes)),
        };
        result.map_err(|e| self.eval_error(e))
    }

    /// The fetch settings the session's fetchers use (Nix's
    /// `fetchSettings`), for parsing flake references as they would.
    pub fn fetch_settings(&self) -> brix_flake::FetchSettings {
        self.with(|ctx| ctx.host.settings.clone())
    }

    /// Parses a flake reference the way the CLI does: relative paths are
    /// taken against `base_dir`. Flake references are text, so a
    /// `base_dir` whose name is not UTF-8 is refused.
    pub fn parse_flake_ref(&self, flake_ref: &str, base_dir: impl AsRef<[u8]>) -> Result<FlakeRef> {
        let opts = ParseOptions {
            base_dir: Some(flake_base_dir(base_dir.as_ref())?.to_owned()),
            ..ParseOptions::flake()
        };
        self.with(|ctx| {
            Ok(brix_flake::parse_flake_ref(
                &ctx.host.settings,
                flake_ref,
                &opts,
            )?)
        })
    }

    /// `parseFlakeRefWithFragment`: a flake reference and its `#fragment`,
    /// relative paths taken against `base_dir`. Unlike an installable, the
    /// text is never read as a store path (`nix bundle --bundler`).
    pub fn parse_flake_ref_with_fragment(
        &self,
        flake_ref: &str,
        base_dir: impl AsRef<[u8]>,
    ) -> Result<(FlakeRef, String)> {
        let opts = ParseOptions {
            base_dir: Some(flake_base_dir(base_dir.as_ref())?.to_owned()),
            ..ParseOptions::flake()
        };
        self.with(|ctx| {
            Ok(brix_flake::parse_flake_ref_with_fragment(
                &ctx.host.settings,
                flake_ref,
                &opts,
            )?)
        })
    }

    /// Fetches, locks (pure mode: the lock file must be complete) and
    /// calls a flake.
    pub fn flake(&self, flake_ref: &FlakeRef) -> Result<Flake<'_>> {
        self.flake_with_flags(flake_ref, &LockFlags::pure(), true)
    }

    /// Like [`Session::flake`] with explicit lock flags (for `flake lock`)
    /// and Nix's `requireLockable`: with `false` the root flake's own
    /// reference is left as written, without a `narHash` (what `nix flake
    /// metadata` shows).
    pub fn flake_with_flags(
        &self,
        flake_ref: &FlakeRef,
        flags: &LockFlags,
        require_lockable: bool,
    ) -> Result<Flake<'_>> {
        let (locked, slot) = self.with(|ctx| {
            let (locked, value) = ctx.ev.with_context(|ev| {
                flake::lock_and_call(ev, ctx.host, flake_ref, flags, require_lockable)
            })?;
            // Explicit compiled images named by input (`nixpkgs=…`) can now
            // be tied to the inputs' content hashes.
            ctx.host
                .images()
                .resolve_names(&locked_input_hashes(&locked.lock_file));
            Ok::<_, Error>((locked, ctx.keep_value(value)))
        })?;
        Ok(Flake {
            session: self,
            slot,
            locked,
            schema_slot: RefCell::new(None),
        })
    }

    /// Fetches and locks a flake, writing its lock file as `flags` say,
    /// without calling it: `lockFlake` alone, which is all `nix flake
    /// metadata`, `lock`, `update` and `prefetch-inputs` do. Calling can
    /// fail where locking does not (a relative input that leaves its tree
    /// for the store directory itself). `require_lockable` as for
    /// [`Session::flake_with_flags`].
    pub fn lock_flake(
        &self,
        flake_ref: &FlakeRef,
        flags: &LockFlags,
        require_lockable: bool,
    ) -> Result<LockedFlake<TreeHandle>> {
        self.with(|ctx| {
            ctx.ev
                .with_context(|ev| flake::lock(ev, ctx.host, flake_ref, flags, require_lockable))
        })
    }

    /// The fingerprint (`LockedFlake::getFingerprint`) of a flake this
    /// session locked, as `nix flake metadata` prints it; `None` when Nix
    /// has none.
    pub fn flake_fingerprint(
        &self,
        locked: &LockedFlake<TreeHandle>,
    ) -> Result<Option<brix_store::Hash>> {
        self.with(|ctx| flake::fingerprint(ctx.host, locked))
    }

    /// The derivations evaluation produced so far.
    pub fn derivations(&self) -> Vec<(StorePath, Derivation)> {
        self.with(|ctx| {
            ctx.ev
                .services()
                .virtual_store()
                .iter()
                .filter_map(|(p, o)| match o {
                    StoreObject::Derivation(d) => Some((p.clone(), d.clone())),
                    _ => None,
                })
                .collect()
        })
    }

    /// Everything evaluation "added to the store": derivations, text
    /// files and source trees with their hashes.
    pub fn store_objects(&self) -> Vec<(StorePath, StoreObject)> {
        self.with(|ctx| {
            ctx.ev
                .services()
                .virtual_store()
                .iter()
                .map(|(p, o)| (p.clone(), o.clone()))
                .collect()
        })
    }

    /// One derivation, if evaluation produced it.
    pub fn derivation(&self, path: &StorePath) -> Option<Derivation> {
        self.with(|ctx| ctx.ev.services().virtual_store().derivation(path).cloned())
    }

    /// The derivation at `path` (`Store::readDerivation`): the one
    /// evaluation produced, else the daemon's copy.
    pub fn read_derivation(&self, path: &StorePath) -> Result<Derivation> {
        self.read_derivation_from_store(path, true)
    }

    /// [`Session::read_derivation`] as `Store::readInvalidDerivation`, for
    /// derivations an operation needs whether or not they are valid
    /// (output paths, `hashDerivationModulo`): one the store does not
    /// have is "store path '…' does not exist".
    pub fn read_invalid_derivation(&self, path: &StorePath) -> Result<Derivation> {
        self.read_derivation_from_store(path, false)
    }

    fn read_derivation_from_store(
        &self,
        path: &StorePath,
        require_valid: bool,
    ) -> Result<Derivation> {
        match self.derivation(path) {
            Some(drv) => Ok(drv),
            None => self.read_store_derivation(path, require_valid),
        }
    }

    /// The derivations at `roots` and, recursively, their input
    /// derivations, each read with [`Session::read_derivation`], sorted by
    /// path (`nix derivation show --recursive`: `computeFSClosure`
    /// restricted to derivations).
    pub fn derivation_closure(&self, roots: &[StorePath]) -> Result<Vec<(StorePath, Derivation)>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        let mut stack: Vec<StorePath> = roots.to_vec();
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            let drv = self.read_derivation(&p)?;
            stack.extend(drv.input_drvs.keys().cloned());
            out.push((p, drv));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// `topoSortPaths` restricted to derivations brix knows: `paths` in
    /// dependency order, dependencies first.
    pub fn sort_derivations(&self, paths: &BTreeSet<StorePath>) -> Vec<StorePath> {
        fn visit(
            s: &Session,
            p: &StorePath,
            wanted: &BTreeSet<StorePath>,
            seen: &mut BTreeSet<StorePath>,
            out: &mut Vec<StorePath>,
        ) {
            if !seen.insert(p.clone()) {
                return;
            }
            if let Some(d) = s.derivation(p) {
                for input in d.input_drvs.keys() {
                    visit(s, input, wanted, seen, out);
                }
            }
            if wanted.contains(p) {
                out.push(p.clone());
            }
        }
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for p in paths {
            visit(self, p, paths, &mut seen, &mut out);
        }
        out
    }

    /// Whether a store (daemon) is configured, reachable or not.
    pub fn has_store(&self) -> bool {
        self.with(|ctx| {
            ctx.host
                .daemon
                .lock()
                .expect("daemon state poisoned")
                .is_configured()
        })
    }

    /// The trees the session fetched or mounted, by store path (flake
    /// inputs, fetched sources, built outputs).
    pub fn tree_accessor(
        &self,
        path: &StorePath,
    ) -> Option<std::sync::Arc<dyn brix_store::SourceAccessor>> {
        self.with(|ctx| ctx.host.tree(path).map(|t| t.accessor))
    }
}

impl<'s> Flake<'s> {
    pub(crate) fn session(&self) -> &'s Session {
        self.session
    }

    pub(crate) fn schema_slot(&self) -> Option<Lease> {
        self.schema_slot.borrow().clone()
    }

    pub(crate) fn set_schema_slot(&self, slot: Lease) {
        *self.schema_slot.borrow_mut() = Some(slot);
    }

    /// The lock and metadata.
    pub fn locked(&self) -> &LockedFlake<TreeHandle> {
        &self.locked
    }

    /// The flake's fingerprint (`LockedFlake::getFingerprint`), as `nix
    /// flake metadata` prints it; `None` when Nix has none.
    pub fn fingerprint(&self) -> Result<Option<brix_store::Hash>> {
        self.session.flake_fingerprint(&self.locked)
    }

    /// The flake's value (`callFlake`'s result): outputs merged with
    /// `inputs`, `outPath`, `sourceInfo`, …
    pub fn root(&self) -> Cursor<'s> {
        Cursor::new(self.session, self.slot.clone(), Vec::new())
    }

    /// The flake's outputs attribute set.
    pub fn outputs(&self) -> Result<Cursor<'s>> {
        self.root().get_attr("outputs")
    }

    /// Evaluates `attr_path` (e.g. `legacyPackages.x86_64-linux.hello`)
    /// in the flake's outputs.
    pub fn eval_attr(&self, attr_path: &str) -> Result<Value> {
        self.attr(attr_path)?.to_owned()
    }

    /// Evaluates `attr_path` and renders it.
    pub fn eval_attr_to_string(&self, attr_path: &str, format: Format) -> Result<String> {
        self.attr(attr_path)?.render(format)
    }

    /// A cursor to `attr_path` in the outputs, or Nix's "does not
    /// provide attribute" error.
    pub fn attr(&self, attr_path: &str) -> Result<Cursor<'s>> {
        let path = crate::cursor::parse_attr_path(attr_path)?;
        match self.outputs()?.find_along_attr_path(&path)? {
            Some(c) => Ok(c),
            None => Err(Error::Usage(
                format!(
                    "flake '{}' does not provide attribute '{attr_path}'",
                    self.locked.flake.original_ref
                )
                .into(),
            )),
        }
    }
}

/// What `nix flake prefetch` reports about a fetched tree.
#[derive(Clone, Debug)]
pub struct Prefetched {
    pub store_path: StorePath,
    pub nar_hash: brix_store::Hash,
    /// The reference after registry resolution.
    pub resolved_ref: FlakeRef,
    /// The reference with the fetched revision and hash filled in.
    pub locked_ref: FlakeRef,
}

impl Session {
    /// Fetches the tree a flake reference denotes, without reading a
    /// `flake.nix` (`nix flake prefetch`).
    pub fn prefetch(&self, flake_ref: &FlakeRef, use_registries: bool) -> Result<Prefetched> {
        self.with(|ctx| {
            let use_registries = if use_registries {
                brix_flake::UseRegistries::All
            } else {
                brix_flake::UseRegistries::No
            };
            let (tree, resolved, _extra) = ctx.ev.with_context(|runtime| {
                flake::Deps::new(runtime, ctx.host).fetch_and_mount(
                    &flake_ref.input,
                    use_registries,
                    true,
                )
            })?;
            let nar_hash = tree
                .input
                .nar_hash()
                .map_err(Error::Flake)?
                .ok_or_else(|| Error::Store("the fetched tree has no NAR hash".into()))?;
            Ok(Prefetched {
                store_path: tree.root.lockable_store_path().clone(),
                nar_hash: store_hash(&nar_hash),
                resolved_ref: FlakeRef::new(resolved, flake_ref.subdir.clone()),
                locked_ref: FlakeRef::new(tree.input, flake_ref.subdir.clone()),
            })
        })
    }

    /// Starts fetching locked inputs in the background, on up to
    /// `http-connections` threads; [`Session::fetch_input`] then waits for
    /// each. For `nix flake prefetch-inputs`, which fetches them all.
    pub fn prefetch_inputs(&self, inputs: &[brix_flake::Input]) {
        self.with(|ctx| ctx.host.prefetch(inputs.iter().cloned()))
    }

    /// Fetches a locked input (for `nix flake prefetch-inputs`).
    pub fn fetch_input(&self, input: &brix_flake::Input) -> Result<StorePath> {
        self.with(|ctx| {
            let (tree, _, _) = ctx.ev.with_context(|runtime| {
                flake::Deps::new(runtime, ctx.host).fetch_and_mount(
                    input,
                    brix_flake::UseRegistries::No,
                    true,
                )
            })?;
            Ok(tree.root.lockable_store_path().clone())
        })
    }
}

impl Session {
    /// Where the contents of a store path evaluation produced can be read:
    /// a source tree added to the virtual store (`builtins.path`, a path
    /// coerced to a string, `flake.nix`'s `./tmpl`), or a fetched tree.
    /// Returns the accessor and the path within it.
    pub fn source_tree(
        &self,
        path: &StorePath,
    ) -> Option<(
        std::sync::Arc<dyn brix_store::SourceAccessor>,
        brix_store::CanonPath,
    )> {
        self.with(|ctx| {
            if let Some(StoreObject::SourceTree { source, .. }) =
                ctx.ev.services().virtual_store().get(path)
            {
                return Some((
                    ctx.ev.services().accessor(source.accessor),
                    source.path.clone(),
                ));
            }
            ctx.host
                .tree(path)
                .map(|t| (t.accessor, brix_store::CanonPath::root()))
        })
    }
}

/// A NAR hash as brix-store's `Hash`.
pub(crate) fn store_hash(nar_hash: &brix_flake::NarHash) -> brix_store::Hash {
    brix_store::Hash::from_bytes(brix_store::HashAlgo::Sha256, nar_hash.as_bytes())
}

/// The directory relative flake references are taken against, as text.
/// Flake references are text in brix, as they are URLs (and Nix fails
/// too, writing the lock file, which is JSON, with such a name), so a
/// directory whose name is not UTF-8 is refused rather than altered.
fn flake_base_dir(base_dir: &[u8]) -> Result<&str> {
    std::str::from_utf8(base_dir).map_err(|_| {
        let mut msg = b"the directory '".to_vec();
        msg.extend_from_slice(base_dir);
        msg.extend_from_slice(
            b"' is not valid UTF-8; brix does not support non-UTF-8 flake references",
        );
        Error::Usage(msg.into())
    })
}

/// Every locked input of a lock file with its content hash, by lock path
/// (`nixpkgs`, `crate2nix/nixpkgs`), for naming compiled images.
fn locked_input_hashes(lock: &brix_flake::LockFile) -> Vec<(String, brix_store::Hash)> {
    let mut out = Vec::new();
    for (path, edge) in lock.all_inputs() {
        let brix_flake::Edge::Node(id) = edge else {
            continue;
        };
        let Some(locked) = &lock.node(id).locked else {
            continue;
        };
        let Ok(Some(nar_hash)) = locked.locked_ref.input.nar_hash() else {
            continue;
        };
        out.push((path.join("/"), store_hash(&nar_hash)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DaemonState;

    /// A session with a store configured but not connected yet, and no
    /// fetching, so nothing outside the test is touched.
    fn session(teardown: Teardown) -> Session {
        let mut config = Config {
            daemon: Some("unix:///nonexistent/brix-session-test".into()),
            teardown,
            ..Config::default()
        };
        config.fetch.enabled = false;
        config.compiled.enabled = false;
        Session::new(config).expect("a session")
    }

    fn collect(session: &Session) {
        session.with(|ctx| ctx.ev.collect().unwrap());
    }

    #[test]
    fn cursor_graphs_release_their_roots_as_owners_are_dropped() {
        let session = session(Teardown::Free);
        let root = session.eval_expr_cursor("{ a = [ 1 2 ]; }", "/").unwrap();
        let clone = root.clone();
        let renamed = root.renamed(vec!["root".into()]);
        collect(&session);
        let list = root.get_attr("a").unwrap();
        let elements = list.get_list().unwrap();
        collect(&session);
        drop(root);
        drop(clone);
        collect(&session);
        drop(renamed);
        drop(list);
        collect(&session);
        assert_eq!(elements[0].get_int().unwrap(), 1);
        assert_eq!(elements[1].get_int().unwrap(), 2);
        drop(elements);
        collect(&session);
        // Reusing vacant slots cannot keep any previous graph rooted.
        for _ in 0..20 {
            let value = session.eval_expr_cursor("[ 3 ]", "/").unwrap();
            collect(&session);
            drop(value);
            collect(&session);
        }
    }

    #[test]
    fn flake_and_schema_owners_release_their_roots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            "{ outputs = { self }: { schemas = {}; answer = 42; }; }",
        )
        .unwrap();
        let session = Session::new(Config::default()).unwrap();
        let flake_ref = session
            .parse_flake_ref(&format!("path:{}", dir.path().display()), "/")
            .unwrap();
        let flake = session.flake(&flake_ref).unwrap();
        collect(&session);
        let schema = crate::schemas::schema_root(&session, &flake, None).unwrap();
        let cached = crate::schemas::schema_root(&session, &flake, None).unwrap();
        collect(&session);
        drop(schema);
        drop(cached);
        collect(&session);
        let root = flake.root();
        let answer = flake.outputs().unwrap().get_attr("answer").unwrap();
        collect(&session);
        drop(flake);
        collect(&session);
        drop(root);
        collect(&session);
        assert_eq!(answer.get_int().unwrap(), 42);
        drop(answer);
        collect(&session);
    }

    #[test]
    fn public_operations_reject_cursors_from_another_session() {
        let a = session(Teardown::Free);
        let b = session(Teardown::Free);
        let function = a.eval_expr_cursor("x: x", "/").unwrap();
        let value = b.eval_expr_cursor("42", "/").unwrap();
        assert!(value.apply(&function).is_err());
        assert!(a.auto_call(&value, &[], "/").is_err());
        assert_eq!(value.get_int().unwrap(), 42);
    }

    /// Dropping a session closes its store whatever the teardown, although
    /// with `Teardown::Leak` the evaluator, which reaches the store
    /// through its accessors, is never dropped.
    #[test]
    fn dropping_a_session_closes_its_store() {
        for teardown in [Teardown::Free, Teardown::Leak] {
            let session = session(teardown);
            assert_eq!(
                session
                    .eval_expr_to_string("builtins.length [ 1 2 ]", "/", Format::Nix)
                    .unwrap(),
                "2"
            );
            let daemon = session.with(|ctx| ctx.host.daemon.clone());
            assert!(matches!(*daemon.lock().unwrap(), DaemonState::Lazy { .. }));
            drop(session);
            assert!(
                matches!(*daemon.lock().unwrap(), DaemonState::Closed),
                "{teardown:?}: the store is still open"
            );
            let other_holders = std::sync::Arc::strong_count(&daemon) - 1;
            match teardown {
                Teardown::Free => assert_eq!(other_holders, 0, "everything was dropped"),
                Teardown::Leak => assert!(other_holders > 0, "the evaluator was leaked"),
            }
        }
    }

    /// `realiseContext` asks a daemon that cannot be reached whether a
    /// store path is valid: the connection error, every time, never
    /// "path … is not valid".
    #[test]
    fn an_unreachable_store_is_an_error_not_an_invalid_path() {
        let path = "/nix/store/00000000000000000000000000000000-brix-unreachable";
        let expr = format!(
            r#"builtins.readFile (builtins.appendContext "{path}" {{ "{path}" = {{ path = true; }}; }})"#
        );
        let session = session(Teardown::Free);
        // The connection error, then that it failed before.
        for expected in [
            "cannot connect to socket at '/nonexistent/brix-session-test': No such file or directory",
            "opening a connection to remote store 'unix:///nonexistent/brix-session-test' previously failed",
        ] {
            let err = session
                .eval_expr_to_string(&expr, "/", Format::Nix)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{err}");
            assert!(!err.contains("is not valid"), "{err}");
        }
        // Without a store, nothing in it is valid.
        let mut config = Config::default();
        config.fetch.enabled = false;
        let session = Session::new(config).unwrap();
        let err = session
            .eval_expr_to_string(&expr, "/", Format::Nix)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("path '{path}' is not valid")),
            "{err}"
        );
    }
}
