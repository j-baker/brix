//! Safe, owning entry point for the precise moving evaluator.
//!
//! Values are owning handles. Clone a handle to retain its graph across calls;
//! dropping its last clone releases that root. Borrowed views prevent collection.
//!
//! ```
//! use brix_eval::{Arena, Config, Evaluator, NullHost};
//! use brix_eval::managed_api::{GcEvaluator, GcValueView};
//! let arena = Arena::new();
//! let host = NullHost;
//! let mut eval = GcEvaluator::new(Evaluator::new(&arena, Config::default(), &host), Default::default()).unwrap();
//! let value = eval.eval_str(b"[ 1 (2 + 3) ]", b"/").unwrap();
//! eval.collect().unwrap();
//! let second = eval.list_item(&value, 1).unwrap().unwrap();
//! eval.force(&second).unwrap();
//! assert!(matches!(eval.view(&second).unwrap(), GcValueView::Int(5)));
//! ```
//!
//! A live byte view excludes mutation of the evaluator:
//!
//! ```compile_fail,E0502
//! use brix_eval::managed_api::{GcEvaluator, GcValue, GcValueView};
//! fn cannot_collect(eval: &mut GcEvaluator<'_>, value: &GcValue) {
//!     let GcValueView::String(text) = eval.view(value).unwrap() else { return };
//!     eval.collect().unwrap();
//!     assert_eq!(text.bytes(), b"hello");
//! }
//! ```
//!
//! The same rule applies to evaluation, even if a collection is only automatic:
//!
//! ```compile_fail,E0502
//! use brix_eval::managed_api::{GcEvaluator, GcValue, GcValueView};
//! fn cannot_evaluate(eval: &mut GcEvaluator<'_>, value: &GcValue) {
//!     let GcValueView::String(text) = eval.view(value).unwrap() else { return };
//!     eval.eval_str(b"1", b"/").unwrap();
//!     assert_eq!(text.bytes(), b"hello");
//! }
//! ```

/// A native callback cannot return a handle captured from an unrelated runtime.
///
/// ```compile_fail
/// use brix_eval::managed_api::{GcEvaluator, GcValue};
/// fn cannot_mix(eval: &mut GcEvaluator<'_>, foreign: GcValue) {
///     eval.register_callback("native", move |_, _, _| Ok(foreign.clone())).unwrap();
/// }
/// ```
const _: () = ();

use crate::runtime::rc::Rc;

use crate::managed_eval::{allocation, Code, Machine};
use crate::managed_heap::{self, CellKind, Heap, Lease, RootedAttr, ValueView};
use crate::managed_language::Language;
use crate::{Error, ErrorKind, EvalError, Evaluator, Result, SrcPos};

pub use brix_gc::{Config as GcConfig, Stats as GcStats};

/// An owning reference to a managed value, independent of the runtime's address.
///
/// A handle may outlive its runtime, but can only be inspected by that runtime.
/// Cloning shares one root; its final drop releases the root without collecting.
#[derive(Clone)]
pub struct GcHandle<'h> {
    identity: Rc<()>,
    cell: Lease<'h, CellKind>,
}

pub type GcValue = GcHandle<'static>;
pub type GcCallValue<'h> = GcHandle<'h>;

impl std::fmt::Debug for GcHandle<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GcValue { .. }")
    }
}

/// Evaluator with an owned, precise moving heap and a statically selected
/// serial or parallel backend.
///
/// Code and host metadata borrow the supplied evaluator's arena and services.
/// Runtime graph storage belongs to this object. Every API accepting a value
/// checks its runtime identity before reading any managed address.
pub struct GcEvaluator<'a> {
    language: Language<'a>,
    machine: Machine<'static>,
    identity: Rc<()>,
    #[cfg(brix_parallel)]
    pool: Option<rayon::ThreadPool>,
}

/// A thread-safe cancellation handle. Cancelling wakes thunk waiters; active
/// workers stop at their next forcing safepoint. Blocking host operations must
/// return before their worker can stop. Cancellation is not a Nix exception.
#[cfg(brix_parallel)]
#[derive(Clone)]
pub struct Cancellation {
    waits: std::sync::Arc<crate::parallel::wait::WaitRegistry>,
}
#[cfg(brix_parallel)]
impl Cancellation {
    pub fn cancel(&self) {
        self.waits.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.waits.is_cancelled()
    }
}

/// Construct a runtime for a callback. Values returned by the callback remain
/// opaque handles after the runtime is dropped; retaining the runtime directly
/// with [`GcEvaluator::new`] permits subsequent evaluation calls.
pub fn with_gc<'a, R>(
    evaluator: Evaluator<'a>,
    config: GcConfig,
    callback: impl FnOnce(&mut GcEvaluator<'a>) -> R,
) -> Result<R> {
    let mut runtime = GcEvaluator::new(evaluator, config)?;
    Ok(callback(&mut runtime))
}

macro_rules! runtime_methods {
    ($heap:lifetime) => {
        /// Run blocking host work on owned data while allowing the common
        /// collector to progress. Heap views cannot cross this exclusive
        /// borrow; captured owning value handles remain rooted.
        pub fn blocking<R>(&mut self, operation: impl FnOnce() -> R) -> R {
            self.machine.heap.blocking(operation)
        }

        /// Immutable compiler metadata and host/store services.
        pub fn services(&self) -> &Evaluator<'a> {
            self.legacy()
        }

        pub fn force_at(&mut self, value: &GcHandle<$heap>, pos: SrcPos) -> Result<()> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                self.language
                    .force(machine, roots, value.cell.root(), pos)?;
                Ok(())
            })
        }
        pub fn force_attrs(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<()> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_builtins::force_attrs(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    ctx,
                )?;
                Ok(())
            })
        }
        pub fn force_string_no_ctx(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<Vec<u8>> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_builtins::force_string_no_ctx(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    ctx,
                )
            })
        }
        pub fn string_bytes(&mut self, value: &GcHandle<$heap>) -> Result<Vec<u8>> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                let value = crate::managed_builtins::force_string(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    SrcPos::NONE,
                    "",
                )?;
                let value = crate::managed_builtins_strings::materialize(
                    &self.language,
                    machine,
                    roots,
                    value,
                )?;
                let ValueView::String(value) = machine.heap.view(value) else {
                    unreachable!()
                };
                Ok(value.bytes().to_vec())
            })
        }
        pub fn coerce_with_context(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            mode: crate::Coercion,
            ctx: &str,
        ) -> Result<GcHandle<$heap>> {
            self.check(value)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = crate::managed_builtins::coerce_with_context(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    mode,
                    ctx,
                )?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn coerce_to_source_path(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<brix_store::SourcePath> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                let (accessor, bytes, _) = crate::managed_builtins_paths::coerce_path(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    ctx,
                )?;
                Ok(brix_store::SourcePath {
                    accessor,
                    path: self.language.legacy().canon_path(&bytes)?,
                })
            })
        }
        pub fn coerce_to_store_path(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<brix_store::StorePath> {
            let value = self.coerce_with_context(value, pos, crate::Coercion::NO_COPY, ctx)?;
            let bytes = self.string_bytes(&value)?;
            self.legacy()
                .config
                .store_dir
                .maybe_parse(&bytes)
                .ok_or_else(|| {
                    self.legacy()
                        .error(
                            ErrorKind::Eval,
                            crate::error::msg!(
                                "cannot coerce '",
                                bytes,
                                "' to a store path because it is not a subpath of the Nix store"
                            ),
                        )
                        .with_trace(self.legacy().position(pos), ctx)
                })
        }
        pub fn show_type(&self, value: &GcHandle<$heap>) -> Result<String> {
            self.check(value)?;
            Ok(self.machine.heap.root_registry().with_roots(|roots| {
                crate::managed_print::describe_type(
                    &self.language,
                    &self.machine,
                    self.machine.heap.value(roots, value.cell.root()),
                )
            }))
        }
        pub fn value_type(&self, value: &GcHandle<$heap>) -> Result<crate::ValueType> {
            self.check(value)?;
            Ok(match self.machine.heap.read(value.cell.root()) {
                ValueView::Int(_) => crate::ValueType::Int,
                ValueView::Float(_) => crate::ValueType::Float,
                ValueView::Bool(_) => crate::ValueType::Bool,
                ValueView::Null => crate::ValueType::Null,
                ValueView::String(_) => crate::ValueType::String,
                ValueView::Path(_) => crate::ValueType::Path,
                ValueView::Attrs(_) => crate::ValueType::Attrs,
                ValueView::List(_) => crate::ValueType::List,
                ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => {
                    crate::ValueType::Function
                }
                _ => crate::ValueType::Thunk,
            })
        }
        pub fn print_value(
            &mut self,
            value: &GcHandle<$heap>,
            options: &crate::PrintOptions,
        ) -> Result<bstr::BString> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_print::print_value(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    options,
                )
            })
        }
        pub fn print_ambiguous(&self, value: &GcHandle<$heap>) -> Result<bstr::BString> {
            self.check(value)?;
            crate::managed_print::print_ambiguous(&self.language, &self.machine, value.cell.root())
        }
        pub fn to_owned_value(&mut self, value: &GcHandle<$heap>) -> Result<crate::OwnedValue> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_print::to_owned_value(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                )
            })
        }
        pub fn force_int(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<i64> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_builtins::force_int(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    ctx,
                )
            })
        }
        pub fn attrs_with_pos(
            &mut self,
            value: &GcHandle<$heap>,
        ) -> Result<Vec<(Vec<u8>, SrcPos, GcHandle<$heap>)>> {
            self.force_attrs(value, SrcPos::NONE, "")?;
            let ValueView::Attrs(attrs) = self.machine.heap.read(value.cell.root()) else {
                unreachable!()
            };
            Ok(attrs
                .iter()
                .map(|attr| {
                    (
                        self.legacy().symbol_name(attr.name).to_vec(),
                        attr.pos,
                        self.retain(attr.value),
                    )
                })
                .collect())
        }
        pub fn is_trivial(&self, value: &GcHandle<$heap>) -> Result<bool> {
            self.check(value)?;
            Ok(match self.machine.heap.read(value.cell.root()) {
                ValueView::Thunk { node, .. } => {
                    let (code, node) = self.legacy().resolve_node(node);
                    match *code.node(node) {
                        crate::ir::Node::Attrs(attrs) => {
                            code.attr_set(attrs).dynamic_attrs.is_empty()
                        }
                        crate::ir::Node::List { .. } | crate::ir::Node::Lambda(_) => true,
                        _ => false,
                    }
                }
                ValueView::App(_) => false,
                _ => true,
            })
        }
        pub fn function_formals(&self, value: &GcHandle<$heap>) -> Result<Option<Vec<Vec<u8>>>> {
            self.check(value)?;
            let ValueView::Lambda { node, .. } = self.machine.heap.read(value.cell.root()) else {
                return Ok(None);
            };
            let (_, lambda) = self.legacy().lambda_def(node);
            Ok(lambda.formals.as_ref().map(|formals| {
                formals
                    .formals
                    .iter()
                    .map(|formal| self.legacy().symbol_name(formal.name).to_vec())
                    .collect()
            }))
        }
        pub fn eval_flake_file(
            &mut self,
            path: &brix_store::SourcePath,
        ) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let path = crate::value::PathVal {
                    accessor: path.accessor,
                    path: path.path.as_bytes(),
                };
                let value =
                    crate::managed_import::file(&self.language, machine, roots, &path, None, true)?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn store_path_string(
            &mut self,
            path: &brix_store::StorePath,
        ) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = crate::managed_builtins_paths::store_path_string(
                    &self.language,
                    machine,
                    roots,
                    path,
                )?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn string_with_context(
            &mut self,
            bytes: &[u8],
            context: &[GcContext],
        ) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            let context = owned_context(context);
            self.machine.with_roots(|machine, roots| {
                let value =
                    crate::managed_builtins_strings::string(machine, roots, bytes, &context)?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        #[cfg(not(brix_parallel))]
        pub fn realise_context(&self, context: &[GcContext], is_ifd: bool) -> Result<()> {
            let context = owned_context(context);
            let borrowed = crate::managed_builtins_strings::borrowed_context(&context)?;
            self.legacy().realise_context(&borrowed, is_ifd)
        }

        #[cfg(brix_parallel)]
        pub fn realise_context(&mut self, context: &[GcContext], is_ifd: bool) -> Result<()> {
            if context.is_empty() {
                return Ok(());
            }
            let owned = owned_context(context);
            let borrowed = crate::managed_builtins_strings::borrowed_context(&owned)?;
            let language = &self.language;
            self.machine
                .heap
                .blocking(|| language.legacy().realise_context(&borrowed, is_ifd))
        }
        pub fn lazy_tree_out_path_thunk(
            &mut self,
            id: crate::LazyTreeId,
        ) -> Result<GcHandle<$heap>> {
            self.lazy_tree_thunk("lazyTreeOutPath", id)
        }
        pub fn lazy_tree_nar_hash_thunk(
            &mut self,
            id: crate::LazyTreeId,
        ) -> Result<GcHandle<$heap>> {
            self.lazy_tree_thunk("lazyTreeNarHash", id)
        }
        fn lazy_tree_thunk(
            &mut self,
            name: &'static str,
            id: crate::LazyTreeId,
        ) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            let primop = self.language.native_primop(name, 1, false);
            self.machine.with_roots(|machine, roots| {
                let fun = machine.heap.primop(roots, primop);
                let fun = allocation(machine.heap.cell(roots, fun))?;
                let arg = allocation(machine.heap.cell(roots, roots.int(i64::from(id.0))))?;
                let app = allocation(machine.heap.app(roots, fun, arg, false))?;
                let cell = allocation(machine.heap.cell(roots, app))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn print_for_error(&self, value: &GcHandle<$heap>) -> Result<Vec<u8>> {
            self.check(value)?;
            Ok(self.machine.heap.root_registry().with_roots(|roots| {
                crate::managed_builtins::print_for_error(
                    &self.language,
                    &self.machine,
                    self.machine.heap.value(roots, value.cell.root()),
                )
            }))
        }
        pub fn auto_call(
            &mut self,
            function: &GcHandle<$heap>,
            arguments: &GcHandle<$heap>,
        ) -> Result<GcHandle<$heap>> {
            self.check(function)?;
            self.check(arguments)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let attrs = crate::managed_builtins::force_attrs(
                    &self.language,
                    machine,
                    roots,
                    arguments.cell.root(),
                    SrcPos::NONE,
                    "while evaluating auto-call arguments",
                )?;
                let value = self
                    .language
                    .auto_call(machine, roots, function.cell.root(), attrs)?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn force_string_parts(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            ctx: &str,
        ) -> Result<(Vec<u8>, Vec<GcContext>)> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                crate::managed_builtins::force_string(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    ctx,
                )?;
                Ok::<(), Error>(())
            })?;
            self.string_parts(value)
        }
        pub fn path_literal_anchor(
            &self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
        ) -> Result<Option<crate::ir::PathAnchor>> {
            self.check(value)?;
            let ValueView::Path(path) = self.machine.heap.read(value.cell.root()) else {
                return Ok(None);
            };
            let Some(node) = path.literal() else {
                return Ok(None);
            };
            let (code, node) = self.legacy().resolve_node(node);
            if self.legacy().locate(pos).map(|(id, _)| id) != Some(code.id) {
                return Ok(None);
            }
            Ok(match *code.node(node) {
                crate::ir::Node::Path { anchor, .. } => Some(anchor),
                _ => None,
            })
        }
        pub fn root_accessor(&self) -> brix_store::AccessorId {
            self.legacy().root_accessor
        }

        fn check(&self, value: &GcHandle<$heap>) -> Result<()> {
            if Rc::ptr_eq(&self.identity, &value.identity) {
                Ok(())
            } else {
                Err(EvalError::new(
                    ErrorKind::Eval,
                    "value belongs to a different evaluator",
                ))
            }
        }

        pub fn eval_str(
            &mut self,
            source: &[u8],
            base_dir: impl AsRef<[u8]>,
        ) -> Result<GcHandle<$heap>> {
            self.eval_source(source, base_dir, crate::Origin::String)
        }

        pub fn eval_source(
            &mut self,
            source: &[u8],
            base_dir: impl AsRef<[u8]>,
            origin: crate::Origin,
        ) -> Result<GcHandle<$heap>> {
            let language = &self.language;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = language.eval_source(machine, roots, source, base_dir, origin)?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        /// Evaluate a command-line file, resolving directories to default.nix.
        pub fn eval_main_file(&mut self, path: &brix_store::SourcePath) -> Result<GcHandle<$heap>> {
            let path = crate::value::PathVal {
                accessor: path.accessor,
                path: path.path.as_bytes(),
            };
            let path = self.legacy().resolve_expr_path(&path)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value =
                    crate::managed_import::main_file(&self.language, machine, roots, &path)?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        /// Import a path through the normal builtin, including its rooted cache.
        pub fn import(&mut self, path: &GcHandle<$heap>) -> Result<GcHandle<$heap>> {
            self.call_builtin("import", path)
        }

        fn call_builtin(
            &mut self,
            name: &str,
            argument: &GcHandle<$heap>,
        ) -> Result<GcHandle<$heap>> {
            self.check(argument)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let function = self.language.builtin(machine, roots, name)?;
                let value = self.language.call(
                    machine,
                    roots,
                    function,
                    argument.cell.root(),
                    SrcPos::NONE,
                )?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        /// Serialize strictly without copying paths into the store.
        pub fn to_json(&mut self, value: &GcHandle<$heap>) -> Result<Vec<u8>> {
            self.to_json_with_options(value, SrcPos::NONE, false)
        }
        /// Direct JSON serialization, preserving source position and path policy.
        /// UTF-8 dump failures remain foreign errors, without builtin wrapping.
        pub fn to_json_with_options(
            &mut self,
            value: &GcHandle<$heap>,
            pos: SrcPos,
            copy_to_store: bool,
        ) -> Result<Vec<u8>> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                let (text, _, error) = crate::managed_builtins_strings::json_undumped_mode(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    pos,
                    copy_to_store,
                )?;
                match error {
                    Some(error) => Err(EvalError::new(ErrorKind::Foreign, error)),
                    None => Ok(text.into_bytes()),
                }
            })
        }

        /// Coerce a value to a managed string, retaining its string context.
        pub fn coerce(
            &mut self,
            value: &GcHandle<$heap>,
            mode: crate::Coercion,
        ) -> Result<GcHandle<$heap>> {
            self.check(value)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let string = crate::managed_builtins::coerce_mode(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    SrcPos::NONE,
                    mode,
                )?;
                let cell = allocation(machine.heap.cell(roots, string))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        /// Force a string and copy its bytes and resolved context out of the heap.
        pub fn string_parts(
            &mut self,
            value: &GcHandle<$heap>,
        ) -> Result<(Vec<u8>, Vec<GcContext>)> {
            self.check(value)?;
            self.machine.with_roots(|machine, roots| {
                use crate::managed_builtins_strings as strings;
                let string = crate::managed_builtins::force_string(
                    &self.language,
                    machine,
                    roots,
                    value.cell.root(),
                    SrcPos::NONE,
                    "while reading a string",
                )?;
                let string = strings::materialize(&self.language, machine, roots, string)?;
                let (bytes, mut context) = strings::snapshot(machine, string);
                strings::resolve_context_parked(&self.language, machine, &mut context)?;
                // Parallel WHNF payloads are immutable after publication.
                // Materialization returns an owned snapshot; lazy host trees
                // have their own shared resolution cache.
                #[cfg(not(brix_parallel))]
                machine.heap.store(value.cell.root(), string);
                let context = context
                    .into_iter()
                    .map(|entry| match entry {
                        strings::OwnedContext::Opaque(path) => GcContext::Opaque(path),
                        strings::OwnedContext::DrvDeep(path) => GcContext::DrvDeep(path),
                        strings::OwnedContext::Built { drv, output } => {
                            GcContext::Built { drv, output }
                        }
                        strings::OwnedContext::Path(path) => GcContext::Path(path),
                        strings::OwnedContext::LazyHash { .. }
                        | strings::OwnedContext::LazyPath(_) => {
                            unreachable!("context is resolved")
                        }
                    })
                    .collect();
                Ok((bytes, context))
            })
        }

        /// Force this cell in place. Existing clones observe the memoized result.
        pub fn force(&mut self, value: &GcHandle<$heap>) -> Result<()> {
            self.check(value)?;
            // An embedder's force is a cancellation point even for a value
            // that is already evaluated; the interpreter's own is not.
            #[cfg(brix_parallel)]
            self.machine.safepoint()?;
            self.machine.with_roots(|machine, roots| {
                self.language
                    .force(machine, roots, value.cell.root(), SrcPos::NONE)?;
                Ok(())
            })
        }

        pub fn apply(
            &mut self,
            function: &GcHandle<$heap>,
            argument: &GcHandle<$heap>,
        ) -> Result<GcHandle<$heap>> {
            self.check(function)?;
            self.check(argument)?;
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = self.language.call(
                    machine,
                    roots,
                    function.cell.root(),
                    argument.cell.root(),
                    SrcPos::NONE,
                )?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        /// Inspect without forcing. The returned view borrows this runtime, and
        /// therefore cannot remain live across a call that may move the heap.
        pub fn view<'v>(&'v self, value: &GcHandle<$heap>) -> Result<GcView<'v, $heap>> {
            self.check(value)?;
            Ok(match self.machine.heap.read(value.cell.root()) {
                ValueView::Int(n) => GcView::Int(n),
                ValueView::Float(n) => GcView::Float(n),
                ValueView::Bool(b) => GcView::Bool(b),
                ValueView::Null => GcView::Null,
                ValueView::String(value) => GcView::String(GcStringView { value }),
                ValueView::Path(value) => GcView::Path {
                    accessor: value.accessor(),
                    bytes: value.bytes(),
                },
                ValueView::List(value) => GcView::List(GcListView {
                    runtime: self,
                    value,
                }),
                ValueView::Attrs(value) => GcView::Attrs(GcAttrsView {
                    runtime: self,
                    value,
                }),
                ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => {
                    GcView::Function
                }
                ValueView::Thunk { .. } | ValueView::App(_) => GcView::Thunk,
                ValueView::Blackhole => GcView::Blackhole,
                ValueView::Failed(error) => GcView::Failed(error),
            })
        }

        /// Force the outer set and retain an attribute cell without forcing it.
        pub fn get_attr(
            &mut self,
            value: &GcHandle<$heap>,
            name: &[u8],
        ) -> Result<Option<GcHandle<$heap>>> {
            self.force(value)?;
            let symbol = self.language.legacy().intern(name);
            match self.machine.heap.read(value.cell.root()) {
                ValueView::Attrs(attrs) => {
                    Ok(attrs.get(symbol).map(|attr| self.retain(attr.value)))
                }
                _ => Err(EvalError::new(ErrorKind::TypeError, "expected a set")),
            }
        }

        /// Force the outer list and retain one element without forcing it.
        pub fn list_item(
            &mut self,
            value: &GcHandle<$heap>,
            index: usize,
        ) -> Result<Option<GcHandle<$heap>>> {
            self.force(value)?;
            match self.machine.heap.read(value.cell.root()) {
                ValueView::List(list) => Ok(list.get(index).map(|cell| self.retain(cell))),
                _ => Err(EvalError::new(ErrorKind::TypeError, "expected a list")),
            }
        }

        fn retain(&self, cell: managed_heap::CellView<'_, $heap>) -> GcHandle<$heap> {
            self.machine
                .heap
                .root_registry()
                .with_roots(|roots| GcHandle {
                    identity: self.identity.clone(),
                    cell: self.machine.heap.persist(cell.root(roots)),
                })
        }

        pub fn int(&mut self, value: i64) -> Result<GcHandle<$heap>> {
            self.scalar(|roots| roots.int(value))
        }
        pub fn float(&mut self, value: f64) -> Result<GcHandle<$heap>> {
            self.scalar(|roots| roots.float(value))
        }
        pub fn bool(&mut self, value: bool) -> Result<GcHandle<$heap>> {
            self.scalar(|roots| roots.bool(value))
        }
        pub fn null(&mut self) -> Result<GcHandle<$heap>> {
            self.scalar(|roots| roots.null())
        }
        fn scalar(
            &mut self,
            value: impl for<'r> FnOnce(
                &managed_heap::Roots<'r, $heap>,
            ) -> managed_heap::ValueRoot<'r, $heap>,
        ) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let cell = allocation(machine.heap.cell(roots, value(roots)))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn string(&mut self, bytes: &[u8]) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = allocation(machine.heap.string(roots, bytes))?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn path(&mut self, path: &brix_store::SourcePath) -> Result<GcHandle<$heap>> {
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let value = allocation(machine.heap.path(
                    roots,
                    path.accessor,
                    path.path.as_bytes(),
                ))?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn list(&mut self, values: &[GcHandle<$heap>]) -> Result<GcHandle<$heap>> {
            for value in values {
                self.check(value)?;
            }
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let cells = values
                    .iter()
                    .map(|value| roots.copy(value.cell.root()))
                    .collect::<Vec<_>>();
                let value = allocation(machine.heap.list(roots, &cells))?;
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }
        pub fn attrs(&mut self, values: &[(&[u8], GcHandle<$heap>)]) -> Result<GcHandle<$heap>> {
            for (_, value) in values {
                self.check(value)?;
            }
            let identity = self.identity.clone();
            self.machine.with_roots(|machine, roots| {
                let mut attrs = values
                    .iter()
                    .map(|(name, value)| RootedAttr {
                        name: self.language.legacy().intern(name),
                        pos: SrcPos::NONE,
                        value: roots.copy(value.cell.root()),
                    })
                    .collect::<Vec<_>>();
                attrs.sort_by_key(|attr| attr.name);
                if attrs.windows(2).any(|pair| pair[0].name == pair[1].name) {
                    return Err(EvalError::new(ErrorKind::Eval, "duplicate attribute name"));
                }
                let attrs = allocation(crate::managed_bindings::from_sorted(
                    &mut machine.heap,
                    roots,
                    &attrs,
                    SrcPos::NONE,
                ))?;
                let value = machine.heap.bindings_value(roots, attrs);
                let cell = allocation(machine.heap.cell(roots, value))?;
                Ok(GcHandle {
                    identity,
                    cell: machine.heap.persist(cell),
                })
            })
        }

        pub fn collect(&mut self) -> Result<()> {
            allocation(self.machine.heap.collect())
        }
        pub fn gc_stats(&self) -> &GcStats {
            self.machine.heap.stats()
        }
        pub fn heap_bytes(&self) -> usize {
            self.machine.heap.used_bytes()
        }
        /// Snapshot of managed spaces: descriptor name, occupied bytes, capacity.
        /// Occupied bytes include unreachable objects until the next collection.
        pub fn heap_space_usage(&self) -> Vec<(&'static str, usize, usize)> {
            self.machine.heap.space_usage()
        }
        /// Reserved scoped-root slot storage, including cached inactive frames.
        /// This subtotal excludes vector roots, persistent handles, registry
        /// bookkeeping and allocator overhead.
        pub fn root_slot_bytes(&self) -> usize {
            self.machine.heap.root_slot_bytes()
        }
    };
}

#[cfg(not(brix_parallel))]
pub(crate) type Callback<'a> = dyn for<'call, 'h> Fn(
        &mut GcCallContext<'call, 'a, 'h>,
        SrcPos,
        &[GcCallValue<'h>],
    ) -> Result<GcCallValue<'h>>
    + 'a;
#[cfg(brix_parallel)]
pub(crate) type Callback<'a> = dyn for<'call, 'h> Fn(
        &mut GcCallContext<'call, 'a, 'h>,
        SrcPos,
        &[GcCallValue<'h>],
    ) -> Result<GcCallValue<'h>>
    + Send
    + Sync
    + 'a;

pub(crate) fn invoke_callback<'a, 'r, 'h>(
    language: &Language<'a>,
    machine: &mut Machine<'h>,
    roots: &managed_heap::Roots<'r, 'h>,
    callback: &Callback<'a>,
    args: &[managed_heap::CellRoot<'_, 'h>],
) -> Result<managed_heap::ValueRoot<'r, 'h>> {
    let identity = machine.identity.clone();
    let args = args
        .iter()
        .map(|arg| GcHandle {
            identity: identity.clone(),
            cell: machine.heap.persist(*arg),
        })
        .collect::<Vec<_>>();
    let mut context = GcCallContext {
        language,
        machine,
        identity,
    };
    let result = callback(&mut context, SrcPos::NONE, &args)?;
    context.check(&result)?;
    Ok(context.machine.heap.value(roots, result.cell.root()))
}

/// A callback's exclusive access to its managed evaluator. Rooted handles carry
/// the heap lifetime; borrowed views exclude all operations that might collect.
pub struct GcCallContext<'call, 'a, 'h> {
    language: &'call Language<'a>,
    machine: &'call mut Machine<'h>,
    identity: Rc<()>,
}
impl<'a, 'h> GcCallContext<'_, 'a, 'h> {
    fn legacy(&self) -> &Evaluator<'a> {
        self.language.legacy()
    }
    runtime_methods!('h);
}

impl<'a> GcEvaluator<'a> {
    #[allow(unsafe_code)]
    pub fn new(evaluator: Evaluator<'a>, config: GcConfig) -> Result<Self> {
        // The fixed brand never crosses this module's public boundary. Every
        // handle carries an unforgeable identity, validated before access.
        // Borrowed views carry a shared runtime borrow, excluding collection.
        #[cfg(not(brix_parallel))]
        let heap = allocation(unsafe { Heap::new_owned(config) })?;
        #[cfg(brix_parallel)]
        let heap = {
            let workers = if evaluator.config.eval_cores == 0 {
                crate::cpus::performance_threads()
            } else {
                evaluator.config.eval_cores as usize
            }
            .max(1);
            if workers >= crate::parallel::publication::MAX_WORKERS as usize {
                return Err(EvalError::new(
                    ErrorKind::Eval,
                    "too many evaluation workers",
                ));
            }
            allocation(unsafe { Heap::new_parallel(config, workers + 1) })?
        };
        let machine = Machine::new(heap);
        let identity = machine.identity.clone();
        Ok(Self {
            language: Language::new(evaluator),
            machine,
            identity,
            #[cfg(brix_parallel)]
            pool: None,
        })
    }

    pub(crate) fn legacy(&self) -> &Evaluator<'a> {
        self.language.legacy()
    }

    runtime_methods!('static);

    #[cfg(brix_parallel)]
    pub fn cancellation(&self) -> Cancellation {
        Cancellation {
            waits: self.machine.heap.cancellation(),
        }
    }

    /// Clear cancellation between batches, after every worker has joined.
    #[cfg(brix_parallel)]
    pub fn clear_cancellation(&mut self) {
        self.machine.heap.cancellation().reset();
    }

    /// Evaluate an explicit batch, preserving input order in the outcomes.
    /// Each job runs synchronously; nested requests through a callback execute
    /// inline. Captured values must belong to this runtime, as for other APIs.
    #[cfg(not(brix_parallel))]
    pub fn evaluate_many<R>(
        &mut self,
        count: usize,
        job: impl Fn(&mut GcCallContext<'_, 'a, 'static>, usize) -> Result<R>,
    ) -> Vec<Result<R>> {
        (0..count)
            .map(|index| self.with_context(|context| job(context, index)))
            .collect()
    }

    #[cfg(brix_parallel)]
    pub fn evaluate_many<R: Send>(
        &mut self,
        count: usize,
        job: impl Fn(&mut GcCallContext<'_, 'a, 'static>, usize) -> Result<R> + Sync,
    ) -> Vec<Result<R>> {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        };
        if count == 0 {
            return Vec::new();
        }
        // Establish one shared builtin environment before workers start.
        if let Err(error) = self
            .machine
            .with_roots(|machine, roots| self.language.base_env(machine, roots).map(|_| ()))
        {
            return (0..count).map(|_| Err(error.clone())).collect();
        }
        let workers = (self.machine.heap.workers() - 1).min(count);
        if workers <= 1 {
            return (0..count)
                .map(|index| {
                    self.with_context(|context| {
                        context.machine.heap.poll()?;
                        job(context, index)
                    })
                })
                .collect();
        }
        if self.pool.is_none() {
            match rayon::ThreadPoolBuilder::new()
                .num_threads(self.machine.heap.workers() - 1)
                .thread_name(|index| format!("brix-eval-{index}"))
                .stack_size(8 * 1024 * 1024)
                .build()
            {
                Ok(pool) => self.pool = Some(pool),
                Err(error) => {
                    return (0..count)
                        .map(|_| {
                            Err(EvalError::new(
                                ErrorKind::Eval,
                                format!("cannot start evaluator workers: {error}"),
                            ))
                        })
                        .collect()
                }
            }
        }
        let next = AtomicUsize::new(0);
        let outcomes = Mutex::new((0..count).map(|_| None).collect::<Vec<_>>());
        let language = &self.language;
        let seeds: Vec<_> = (1..=workers)
            .map(|index| {
                (
                    self.machine.heap.attachment(index),
                    self.machine.identity.clone(),
                    self.machine.base_env.clone(),
                    self.machine.uninit.clone(),
                    self.machine.empty_list.clone(),
                    self.machine.internal.clone(),
                    self.machine.file_cache.clone(),
                    self.machine.imports.clone(),
                    self.machine.shared_file_cache.clone(),
                )
            })
            .collect();
        // Joining threads is itself a blocking operation. The coordinator's
        // roots stay registered while its mutator is parked for the batch.
        let _parked = self.machine.heap.park();
        self.pool
            .as_ref()
            .expect("worker pool initialized")
            .scope(|scope| {
                for (
                    attachment,
                    identity,
                    base_env,
                    uninit,
                    empty_list,
                    internal,
                    file_cache,
                    imports,
                    shared_file_cache,
                ) in seeds
                {
                    let job = &job;
                    let next = &next;
                    let outcomes = &outcomes;
                    scope.spawn(move |_| {
                        let heap = match allocation(attachment.attach()) {
                            Ok(heap) => heap,
                            Err(error) => {
                                loop {
                                    let index = next.fetch_add(1, Ordering::Relaxed);
                                    if index >= count {
                                        break;
                                    }
                                    outcomes.lock().unwrap()[index] = Some(Err(error.clone()));
                                }
                                return;
                            }
                        };
                        let mut machine = Machine::new(heap);
                        machine.identity = identity.clone();
                        machine.base_env = base_env;
                        machine.uninit = uninit;
                        machine.empty_list = empty_list;
                        machine.internal = internal;
                        machine.file_cache = file_cache;
                        machine.imports = imports;
                        machine.shared_file_cache = shared_file_cache;
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            if index >= count {
                                break;
                            }
                            let mut context = GcCallContext {
                                language,
                                machine: &mut machine,
                                identity: identity.clone(),
                            };
                            let result = context
                                .machine
                                .heap
                                .poll()
                                .and_then(|()| job(&mut context, index));
                            outcomes.lock().unwrap()[index] = Some(result);
                        }
                    });
                }
            });
        outcomes
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|result| result.expect("every batch job completed"))
            .collect()
    }

    /// Discover jobs on the calling mutator while workers continuously consume
    /// a bounded queue. Reports run serially on completing workers, in completion
    /// order, with their mutators parked. Returning false stops admission and
    /// further reports; already running jobs finish before this call returns.
    /// Neither callback may reenter this evaluator through another reference.
    #[cfg(brix_parallel)]
    pub fn evaluate_stream<T: Send, R: Send>(
        &mut self,
        mut next: impl FnMut(&mut GcCallContext<'_, 'a, 'static>) -> Result<Option<T>>,
        job: impl Fn(&mut GcCallContext<'_, 'a, 'static>, T) -> Result<R> + Sync,
        report: impl FnMut(Result<R>) -> bool + Send,
    ) -> Result<()> {
        use crate::parallel::jobs::{Queue, Work};
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        use std::sync::Mutex;

        self.machine
            .with_roots(|machine, roots| self.language.base_env(machine, roots).map(|_| ()))?;
        let workers = self.machine.heap.workers() - 1;
        if self.pool.is_none() {
            self.pool = Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .thread_name(|index| format!("brix-eval-{index}"))
                    .stack_size(8 * 1024 * 1024)
                    .build()
                    .map_err(|error| {
                        EvalError::new(
                            ErrorKind::Eval,
                            format!("cannot start evaluator workers: {error}"),
                        )
                    })?,
            );
        }
        let queue = Queue::new(workers.saturating_mul(4), workers);
        let report = Mutex::new(report);
        let report = &report;
        let queue = &queue;
        let job = &job;
        let language = &self.language;
        let machine = &mut self.machine;
        self.pool.as_ref().unwrap().in_place_scope(|scope| {
            for index in 1..=workers {
                let attachment = machine.heap.attachment(index);
                let identity = machine.identity.clone();
                let base_env = machine.base_env.clone();
                let uninit = machine.uninit.clone();
                let empty_list = machine.empty_list.clone();
                let internal = machine.internal.clone();
                let file_cache = machine.file_cache.clone();
                let imports = machine.imports.clone();
                let shared_file_cache = machine.shared_file_cache.clone();
                scope.spawn(move |_| {
                    // Declared first: signal completion only after the worker's
                    // heap and temporary roots have been detached, even on panic.
                    let _worker = queue.worker();
                    let heap = match allocation(attachment.attach()) {
                        Ok(heap) => heap,
                        Err(error) => {
                            let mut report = report.lock().unwrap();
                            if !queue.stopped() {
                                report(Err(error));
                                queue.stop();
                            }
                            return;
                        }
                    };
                    let mut machine = Machine::new(heap);
                    machine.identity = identity.clone();
                    machine.base_env = base_env;
                    machine.uninit = uninit;
                    machine.empty_list = empty_list;
                    machine.internal = internal;
                    machine.file_cache = file_cache;
                    machine.imports = imports;
                    machine.shared_file_cache = shared_file_cache;
                    machine.sparks = Some(queue.sparks.clone());
                    while let Some(work) = machine.heap.blocking(|| queue.pop()) {
                        let input = match work {
                            Work::Spark(spark) => {
                                queue.sparks.run(language, &mut machine, spark);
                                continue;
                            }
                            Work::Primary(input) => input,
                        };
                        let _primary = queue.primary();
                        let mut context = GcCallContext {
                            language,
                            machine: &mut machine,
                            identity: identity.clone(),
                        };
                        let result = context
                            .machine
                            .heap
                            .poll()
                            .and_then(|()| job(&mut context, input));
                        machine.heap.blocking(|| {
                            let mut report = report.lock().unwrap();
                            if !queue.stopped() && !report(result) {
                                queue.stop();
                            }
                        });
                    }
                });
            }
            // Always close and park before joining, including if discovery
            // panics. Otherwise idle workers or a collector could wait forever.
            let discovered = catch_unwind(AssertUnwindSafe(|| {
                while !queue.stopped() {
                    machine.heap.poll()?;
                    let mut context = GcCallContext {
                        language,
                        identity: machine.identity.clone(),
                        machine,
                    };
                    let Some(input) = next(&mut context)? else {
                        break;
                    };
                    if !machine.heap.blocking(|| queue.push(input)) {
                        break;
                    }
                }
                Ok(())
            }));
            if discovered.is_err() {
                queue.stop();
            }
            queue.close();
            machine.heap.blocking(|| queue.join());
            match discovered {
                Ok(result) => result,
                Err(panic) => resume_unwind(panic),
            }
        })
    }

    /// Force precisely the supplied values to WHNF; list elements and
    /// attribute values remain lazy unless supplied individually.
    pub fn force_many(&mut self, values: &[GcValue]) -> Vec<Result<()>> {
        self.evaluate_many(values.len(), |context, index| context.force(&values[index]))
    }

    /// Attach a managed implementation to a primop registered during metadata
    /// construction. Registration finishes before the first evaluation.
    pub fn register_callback<F>(&mut self, name: &str, callback: F) -> Result<()>
    where
        F: for<'call, 'h> Fn(
                &mut GcCallContext<'call, 'a, 'h>,
                SrcPos,
                &[GcCallValue<'h>],
            ) -> Result<GcCallValue<'h>>
            + crate::runtime::ThreadSafe
            + 'a,
    {
        if self.machine.base_env.is_some() {
            return Err(EvalError::new(
                ErrorKind::Eval,
                "callbacks must be registered before evaluation",
            ));
        }
        self.language
            .register_callback(name, crate::runtime::rc::Rc::new(callback))
    }

    pub fn with_context<R>(
        &mut self,
        callback: impl FnOnce(&mut GcCallContext<'_, 'a, 'static>) -> R,
    ) -> R {
        let mut context = GcCallContext {
            language: &self.language,
            machine: &mut self.machine,
            identity: self.identity.clone(),
        };
        callback(&mut context)
    }
}

/// Owned string context. Store paths are their base names, without store-dir.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GcContext {
    Opaque(Vec<u8>),
    DrvDeep(Vec<u8>),
    Built { drv: Vec<u8>, output: Vec<u8> },
    Path(Vec<u8>),
}

fn owned_context(context: &[GcContext]) -> Vec<crate::managed_builtins_strings::OwnedContext> {
    use crate::managed_builtins_strings::OwnedContext as C;
    context
        .iter()
        .map(|c| match c {
            GcContext::Opaque(p) => C::Opaque(p.clone()),
            GcContext::DrvDeep(p) => C::DrvDeep(p.clone()),
            GcContext::Built { drv, output } => C::Built {
                drv: drv.clone(),
                output: output.clone(),
            },
            GcContext::Path(p) => C::Path(p.clone()),
        })
        .collect()
}

pub type GcValueView<'v> = GcView<'v, 'static>;
pub type GcCallValueView<'v, 'h> = GcView<'v, 'h>;

/// A view of the current cell state. It does not force lazy values.
#[derive(Clone, Copy)]
pub enum GcView<'v, 'h> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String(GcStringView<'v, 'h>),
    Path {
        accessor: brix_store::AccessorId,
        bytes: &'v [u8],
    },
    List(GcListView<'v, 'h>),
    Attrs(GcAttrsView<'v, 'h>),
    Function,
    Thunk,
    Blackhole,
    Failed(&'v Error),
}

#[derive(Clone, Copy)]
pub struct GcStringView<'v, 'h> {
    value: managed_heap::StringView<'v, 'h>,
}
impl<'v, 'h> GcStringView<'v, 'h> {
    pub fn bytes(self) -> &'v [u8] {
        self.value.bytes()
    }
    pub fn has_context(self) -> bool {
        self.value.contexts().any(|context| {
            matches!(
                context,
                managed_heap::ContextView::Opaque(_)
                    | managed_heap::ContextView::DrvDeep(_)
                    | managed_heap::ContextView::Built { .. }
            )
        })
    }
}

#[derive(Clone, Copy)]
pub struct GcListView<'v, 'h> {
    runtime: &'v dyn ViewOwner<'h>,
    value: managed_heap::ListView<'v, 'h>,
}
impl<'h> GcListView<'_, 'h> {
    pub fn len(self) -> usize {
        self.value.len()
    }
    pub fn is_empty(self) -> bool {
        self.value.is_empty()
    }
    pub fn get(self, index: usize) -> Option<GcHandle<'h>> {
        self.value.get(index).map(|cell| self.runtime.retain(cell))
    }
}

#[derive(Clone, Copy)]
pub struct GcAttrsView<'v, 'h> {
    runtime: &'v dyn ViewOwner<'h>,
    value: managed_heap::BindingsView<'v, 'h>,
}
impl<'v, 'h> GcAttrsView<'v, 'h> {
    pub fn len(self) -> usize {
        self.value.len()
    }
    pub fn is_empty(self) -> bool {
        self.value.is_empty()
    }
    pub fn get(self, name: &[u8]) -> Option<GcHandle<'h>> {
        let symbol = self.runtime.intern(name);
        self.value
            .get(symbol)
            .map(|attr| self.runtime.retain(attr.value))
    }
    pub fn names(self) -> impl Iterator<Item = &'v [u8]> + 'v + use<'v, 'h> {
        self.value
            .iter()
            .map(move |attr| self.runtime.symbol_name(attr.name))
    }
    pub fn get_with_pos(self, name: &[u8]) -> Option<(GcHandle<'h>, SrcPos)> {
        let name = self.runtime.intern(name);
        self.value
            .get(name)
            .map(|attr| (self.runtime.retain(attr.value), attr.pos))
    }
    pub fn iter(self) -> impl Iterator<Item = (&'v [u8], GcHandle<'h>)> + 'v {
        self.value.iter().map(move |attr| {
            (
                self.runtime.symbol_name(attr.name),
                self.runtime.retain(attr.value),
            )
        })
    }
}

// Erases only the immutable metadata lifetime from borrowed views. All heap
// pointers remain behind their original runtime borrow; no lifetime is extended.
trait ViewOwner<'h> {
    fn retain(&self, cell: managed_heap::CellView<'_, 'h>) -> GcHandle<'h>;
    fn intern(&self, name: &[u8]) -> crate::Symbol;
    fn symbol_name(&self, name: crate::Symbol) -> &[u8];
}
impl ViewOwner<'static> for GcEvaluator<'_> {
    fn retain(&self, cell: managed_heap::CellView<'_, 'static>) -> GcValue {
        self.retain(cell)
    }
    fn intern(&self, name: &[u8]) -> crate::Symbol {
        self.language.legacy().intern(name)
    }
    fn symbol_name(&self, name: crate::Symbol) -> &[u8] {
        self.language.legacy().symbol_name(name)
    }
}

impl<'h> ViewOwner<'h> for GcCallContext<'_, '_, 'h> {
    fn retain(&self, cell: managed_heap::CellView<'_, 'h>) -> GcHandle<'h> {
        self.retain(cell)
    }
    fn intern(&self, name: &[u8]) -> crate::Symbol {
        self.legacy().intern(name)
    }
    fn symbol_name(&self, name: crate::Symbol) -> &[u8] {
        self.legacy().symbol_name(name)
    }
}

#[cfg(all(test, brix_parallel))]
#[path = "parallel/sparks_tests.rs"]
mod sparks_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arena, Config, NullHost};

    fn runtime<'a>(arena: &'a Arena, host: &'a NullHost) -> GcEvaluator<'a> {
        GcEvaluator::new(
            Evaluator::new(arena, Config::default(), host),
            GcConfig {
                workers: 0,
                chunk_bytes: 256,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 1024,
                stress: true,
            },
        )
        .unwrap()
    }

    #[test]
    fn persistent_functions_and_lazy_children_survive_later_calls() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let function = eval
            .eval_str(
                b"let captured = [40 2]; in ignored: builtins.foldl' builtins.add 0 captured",
                b"/",
            )
            .unwrap();
        let cloned = function.clone();
        drop(function);
        let ignored = eval.null().unwrap();
        for _ in 0..8 {
            let other = eval.eval_str(b"{ junk = [1 2 3]; }", b"/").unwrap();
            drop(other);
            eval.collect().unwrap();
        }
        let result = eval.apply(&cloned, &ignored).unwrap();
        assert!(matches!(eval.view(&result).unwrap(), GcValueView::Int(42)));
        let list = eval
            .eval_str(b"[ (20 + 22) (throw \"must stay lazy\") ]", b"/")
            .unwrap();
        let GcValueView::List(view) = eval.view(&list).unwrap() else {
            panic!("list")
        };
        let item = view.get(0).unwrap();
        drop(list);
        eval.collect().unwrap();
        eval.force(&item).unwrap();
        assert!(matches!(eval.view(&item).unwrap(), GcValueView::Int(42)));
    }

    #[test]
    fn final_handle_drop_reclaims_large_shared_graph() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let large = eval.string(&vec![b'x'; 32_768]).unwrap();
        let clone = large.clone();
        let parent = eval.list(&[large]).unwrap();
        eval.collect().unwrap();
        let live = eval.heap_bytes();
        assert!(live >= 32_768);
        drop(clone);
        eval.collect().unwrap();
        assert_eq!(eval.heap_bytes(), live);
        let child = eval.list_item(&parent, 0).unwrap().unwrap();
        drop(parent);
        eval.collect().unwrap();
        let GcValueView::String(text) = eval.view(&child).unwrap() else {
            panic!("string")
        };
        assert_eq!(text.bytes().len(), 32_768);
        drop(child);
        eval.collect().unwrap();
        assert_eq!(eval.heap_bytes(), 0);
        assert!(eval.gc_stats().reclaimed_bytes >= 32_768);
    }

    #[test]
    fn every_handle_entry_rejects_foreign_or_destroyed_runtime() {
        let arena = Arena::new();
        let host = NullHost;
        let mut first = runtime(&arena, &host);
        let mut second = runtime(&arena, &host);
        let foreign = first.string(b"foreign").unwrap();
        let local = second.int(1).unwrap();
        assert!(second.view(&foreign).is_err());
        assert!(second.force(&foreign).is_err());
        assert!(second.apply(&foreign, &local).is_err());
        assert!(second.apply(&local, &foreign).is_err());
        assert!(second.get_attr(&foreign, b"x").is_err());
        assert!(second.list_item(&foreign, 0).is_err());
        assert!(second.list(std::slice::from_ref(&foreign)).is_err());
        assert!(second.attrs(&[(b"x", foreign.clone())]).is_err());
        assert!(second.string_parts(&foreign).is_err());
        assert!(second.import(&foreign).is_err());
        assert!(second.to_json(&foreign).is_err());
        assert!(second.coerce(&foreign, crate::Coercion::NO_COPY).is_err());
        drop(first);
        assert!(second.view(&foreign).is_err());
        drop(foreign);
        second.collect().unwrap();
        assert!(matches!(second.view(&local).unwrap(), GcValueView::Int(1)));
    }

    #[test]
    fn attribute_views_retain_children_and_preserve_byte_names() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let value = eval.int(42).unwrap();
        let set = eval.attrs(&[(b"\xff", value)]).unwrap();
        let GcValueView::Attrs(attrs) = eval.view(&set).unwrap() else {
            panic!("attrs")
        };
        let pairs = attrs
            .iter()
            .map(|(name, value)| (name.to_vec(), value))
            .collect::<Vec<_>>();
        assert_eq!(pairs[0].0, b"\xff");
        let child = attrs.get(b"\xff").unwrap();
        drop(set);
        eval.collect().unwrap();
        assert!(matches!(eval.view(&child).unwrap(), GcValueView::Int(42)));
        assert!(eval.gc_stats().collections > 0);
    }
    #[test]
    fn serialization_coercion_and_resolved_context_use_managed_values() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let value = eval
            .eval_str(b"{ z = [1 true null]; a = 2 + 3; }", b"/")
            .unwrap();
        assert_eq!(
            eval.to_json(&value).unwrap(),
            br#"{"a":5,"z":[1,true,null]}"#
        );
        let number = eval.int(42).unwrap();
        let text = eval.coerce(&number, crate::Coercion::MORE).unwrap();
        assert_eq!(eval.string_parts(&text).unwrap(), (b"42".to_vec(), vec![]));
        let text = eval.eval_str(br#"builtins.appendContext "x" { "/nix/store/00000000000000000000000000000000-test" = { path = true; }; }"#, b"/").unwrap();
        let GcValueView::String(view) = eval.view(&text).unwrap() else {
            panic!("string")
        };
        assert!(view.has_context());
        assert_eq!(
            eval.string_parts(&text).unwrap().1,
            vec![GcContext::Opaque(
                b"00000000000000000000000000000000-test".to_vec()
            )]
        );
    }
    #[test]
    fn imports_and_main_files_use_managed_cache_and_byte_paths() {
        use brix_store::{CanonPath, MemoryAccessor, SourcePath};
        let arena = Arena::new();
        let host = NullHost;
        let legacy = Evaluator::new(&arena, Config::default(), &host);
        let mut files = MemoryAccessor::new();
        files
            .add_file(
                &CanonPath::new("/pkg/default.nix").unwrap(),
                b"{ answer = 40 + 2; }",
                false,
            )
            .unwrap();
        files
            .add_file(
                &CanonPath::new(b"/caf\xff.nix").unwrap(),
                b"{ answer = 20 + 22; }",
                false,
            )
            .unwrap();
        legacy.mount(
            CanonPath::new("/fixtures").unwrap(),
            std::sync::Arc::new(files),
        );
        legacy.allow_path(CanonPath::new("/fixtures").unwrap());
        let mut eval = GcEvaluator::new(
            legacy,
            GcConfig {
                stress: true,
                ..Default::default()
            },
        )
        .unwrap();
        let directory = SourcePath {
            accessor: eval.root_accessor(),
            path: CanonPath::new("/fixtures/pkg").unwrap(),
        };
        let main = eval.eval_main_file(&directory).unwrap();
        assert_eq!(eval.to_json(&main).unwrap(), br#"{"answer":42}"#);
        let path = SourcePath {
            accessor: eval.root_accessor(),
            path: CanonPath::new(b"/fixtures/caf\xff.nix").unwrap(),
        };
        let path = eval.path(&path).unwrap();
        let imported = eval.import(&path).unwrap();
        drop(imported);
        eval.collect().unwrap();
        let imported = eval.import(&path).unwrap();
        assert_eq!(eval.to_json(&imported).unwrap(), br#"{"answer":42}"#);
    }
    fn unavailable_legacy_callback<'a>(
        _: &Evaluator<'a>,
        _: SrcPos,
        _: &[crate::ValueRef<'a>],
    ) -> Result<crate::Value<'a>> {
        panic!("managed callbacks must never invoke the legacy evaluator")
    }

    #[test]
    fn native_callbacks_reenter_collect_and_return_rooted_graphs() {
        let arena = Arena::new();
        let host = NullHost;
        let legacy = Evaluator::with_builtins(&arena, Config::default(), &host, |ev| {
            ev.add_primop("__nativeReenter", 1, &unavailable_legacy_callback);
            ev.add_primop("__nativeBuild", 1, &unavailable_legacy_callback);
        });
        let mut eval = GcEvaluator::new(
            legacy,
            GcConfig {
                stress: true,
                chunk_bytes: 256,
                ..Default::default()
            },
        )
        .unwrap();
        eval.register_callback("nativeReenter", |cx, pos, args| {
            let n = cx.force_int(&args[0], pos, "native argument")?;
            if n == 0 {
                return cx.int(42);
            }
            let callback = cx.eval_str(b"builtins.nativeReenter", b"/")?;
            let argument = cx.int(n - 1)?;
            let result = cx.apply(&callback, &argument)?;
            cx.collect()?;
            Ok(result)
        })
        .unwrap();
        eval.register_callback("nativeBuild", |cx, _, args| {
            let call = cx.eval_str(b"x: { result = x + 2; ignored = throw \"unused\"; }", b"/")?;
            let value = cx.apply(&call, &args[0])?;
            cx.collect()?;
            Ok(value)
        })
        .unwrap();
        let value = eval.eval_str(b"builtins.nativeReenter 5", b"/").unwrap();
        assert!(matches!(eval.view(&value).unwrap(), GcValueView::Int(42)));
        let value = eval.eval_str(b"builtins.nativeBuild 40", b"/").unwrap();
        let child = eval
            .with_context(|cx| cx.get_attr(&value, b"result"))
            .unwrap()
            .unwrap();
        drop(value);
        eval.collect().unwrap();
        eval.force(&child).unwrap();
        assert!(matches!(eval.view(&child).unwrap(), GcValueView::Int(42)));
        assert!(eval
            .register_callback("nativeBuild", |cx, _, _| cx.null())
            .is_err());
    }

    #[test]
    fn auto_call_and_printers_keep_managed_semantics() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let function = eval.eval_str(b"{ x, y ? 2 }: x + y", b"/").unwrap();
        let x = eval.int(40).unwrap();
        let args = eval.attrs(&[(b"x", x)]).unwrap();
        let result = eval.auto_call(&function, &args).unwrap();
        assert_eq!(
            eval.print_value(&result, &crate::PrintOptions::for_eval())
                .unwrap()
                .as_slice(),
            b"42"
        );
        assert_eq!(
            eval.to_owned_value(&result).unwrap(),
            crate::OwnedValue::Int(42)
        );
        let empty = eval.eval_str(b"[ {} {} ]", b"/").unwrap();
        assert_eq!(
            eval.print_value(&empty, &crate::PrintOptions::for_eval())
                .unwrap()
                .as_slice(),
            "[ { } «repeated» ]".as_bytes()
        );
    }
    #[test]
    fn direct_json_preserves_foreign_utf8_dump_errors() {
        let arena = Arena::new();
        let host = NullHost;
        let mut eval = runtime(&arena, &host);
        let value = eval.string(b"\xff").unwrap();
        let actual = eval.to_json(&value).unwrap_err();
        let legacy = eval.legacy();
        let cell = legacy.alloc_cell(legacy.alloc_string_value(b"\xff", &[]));
        let expected = legacy
            .to_json(
                cell,
                SrcPos::NONE,
                &mut crate::string_context::StringContext::new(),
                true,
                false,
            )
            .unwrap_err();
        assert_eq!(actual.kind, ErrorKind::Foreign);
        assert_eq!(actual.render(true), expected.render(true));
    }
    #[test]
    fn path_literal_provenance_survives_aliases_and_moving_collection() {
        use brix_store::{CanonPath, MemoryAccessor, SourcePath};
        let arena = Arena::new();
        let host = NullHost;
        let legacy = Evaluator::new(&arena, Config::default(), &host);
        let mut files = MemoryAccessor::new();
        files.add_file(&CanonPath::new("/paths.nix").unwrap(), br#"rec { literal = ./target; alias = literal; computed = literal + ""; absolute = /elsewhere; }"#, false).unwrap();
        legacy.mount(
            CanonPath::new("/fixtures").unwrap(),
            std::sync::Arc::new(files),
        );
        legacy.allow_path(CanonPath::new("/fixtures").unwrap());
        let mut eval = GcEvaluator::new(
            legacy,
            GcConfig {
                stress: true,
                ..Default::default()
            },
        )
        .unwrap();
        let path = SourcePath {
            accessor: eval.root_accessor(),
            path: CanonPath::new("/fixtures/paths.nix").unwrap(),
        };
        let set = eval.eval_main_file(&path).unwrap();
        for (name, expected) in [
            (b"literal".as_slice(), Some(crate::ir::PathAnchor::Source)),
            (b"alias", Some(crate::ir::PathAnchor::Source)),
            (b"computed", None),
            (b"absolute", Some(crate::ir::PathAnchor::Root)),
        ] {
            let GcValueView::Attrs(attrs) = eval.view(&set).unwrap() else {
                panic!("attrs")
            };
            let (path, pos) = attrs.get_with_pos(name).unwrap();
            eval.force(&path).unwrap();
            eval.collect().unwrap();
            assert_eq!(
                eval.path_literal_anchor(&path, pos).unwrap(),
                expected,
                "{:?}",
                name
            );
        }
    }
}
