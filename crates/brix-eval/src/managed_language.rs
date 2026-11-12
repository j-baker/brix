//! Closure-compiled Nix expressions over the rooted moving heap.
//!
//! The legacy evaluator supplies immutable code, symbols, positions and host
//! services. Its evaluation entry points are never called by this engine.
#![allow(dead_code)]

use crate::runtime::cell::{Cell, RefCell};
use crate::runtime::rc::Rc;
use crate::runtime::Published;
use std::collections::HashMap;

use crate::code::LinkedCode;
use crate::error::{msg, ErrorKind};
use crate::ir::{AttrKind, AttrName, BinOp, Node, NodeId, Origin, VarRef};
use crate::managed_bindings as bindings;
use crate::managed_eval::{allocation, Code, Machine};
use crate::managed_heap::{
    BindingsRoot, CellRoot, ContextInput, EnvRoot, FlatLazyPlan, FlatThunkPlan, LazyAttr,
    LazyAttrSource, RootedAttr, Roots, ThunkAttr, ValueRoot, ValueView,
};
use crate::symbol::{sym, Symbol};
use crate::value::{ContextElem, Env, GlobalNode, SrcPos, Value, ValueRef};
use crate::{Evaluator, Result};

#[cfg(not(brix_parallel))]
type Eval<'a> = &'a (dyn for<'r, 'h, 'e> Fn(
    &Language<'a>,
    &mut Machine<'h>,
    &Roots<'r, 'h>,
    EnvRoot<'e, 'h>,
) -> Result<ValueRoot<'r, 'h>>
         + 'a);
#[cfg(brix_parallel)]
type Eval<'a> = &'a (dyn for<'r, 'h, 'e> Fn(
    &Language<'a>,
    &mut Machine<'h>,
    &Roots<'r, 'h>,
    EnvRoot<'e, 'h>,
) -> Result<ValueRoot<'r, 'h>>
         + Send
         + Sync
         + 'a);

/// Builtins a background spark leaves to the ordered consumer: those whose
/// output the user sees in evaluation order, and those that fetch. Builds
/// (import from derivation) and warnings inside other builtins stop a spark
/// where they happen (`Machine::ordered_effect`). Reading source files and
/// adding paths or derivations to the store are neither ordered nor visible.
#[cfg(brix_parallel)]
fn ordered_builtin(name: &str) -> bool {
    matches!(
        name,
        "trace"
            | "traceVerbose"
            | "warn"
            | "break"
            | "fetchTree"
            | "fetchGit"
            | "fetchurl"
            | "fetchTarball"
            | "fetchMercurial"
            | "fetchClosure"
            | "getFlake"
            | "exec"
            | "importNative"
    )
}

#[derive(Clone, Copy)]
struct Primop<'a> {

    name: &'a str,
    dispatch_name: &'a str,
    handler: crate::managed_builtins::Handler,
    arity: u8,
    add_trace: bool,
    has_callback: bool,
}

/// Dense language-local metadata index; each Machine owns its own rooted cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiteralId(u32);

#[derive(Clone, Copy)]
enum Lazy {
    EmptyList,
    Literal {
        node: NodeId,
        id: LiteralId,
    },
    Variable {
        node: NodeId,
        level: u32,
        displ: u32,
    },
    Thunk(NodeId),
}

type ClosureSlots<'a> = &'a [Published<'a, Option<Eval<'a>>>];

/// Immutable code metadata, shared by every heap instance of a lambda. The
/// body stays lazy until argument matching succeeds on the first call.
struct LambdaDispatch<'a> {
    code: &'a LinkedCode<'a>,
    definition: &'a crate::code::Lambda<'a>,
    body: Published<'a, Option<Eval<'a>>>,
}

type LambdaDispatchSlots<'a> = &'a [Cell<Option<&'a LambdaDispatch<'a>>>];

pub(crate) struct Language<'a> {
    legacy: Evaluator<'a>,
    // Only file-base pages have entries; a file's slots cover its actual nodes,
    // not the unused tail of its global node pages. Both levels are arena-stable.
    closures: Published<'a, &'a [Published<'a, ClosureSlots<'a>>]>,
    // Direct GlobalNode pages alias slices of the existing per-file closure
    // slots. They add no per-node evaluator storage and remain arena-stable.
    thunk_dispatch: Published<'a, &'a [Published<'a, ClosureSlots<'a>>]>,
    // GlobalNode pages point directly at prepared lambda descriptors. Pages
    // are allocated only on demand; descriptors and slot slices never move.
    lambda_dispatch: Published<'a, &'a [Published<'a, LambdaDispatchSlots<'a>>]>,
    literal_ids: RefCell<HashMap<u32, LiteralId>>,
    primops: RefCell<Vec<Primop<'a>>>,
    // Every call of a builtin reads its entry. The parallel build reads an
    // immutable snapshot instead of taking the table's (contended) read lock.
    #[cfg(brix_parallel)]
    primop_table: Published<'a, &'a [Primop<'a>]>,
    primop_ids: RefCell<HashMap<usize, usize>>,
    callbacks: RefCell<HashMap<String, Rc<crate::managed_api::Callback<'a>>>>,
    #[cfg(brix_parallel)]
    file_codes: RefCell<HashMap<(brix_store::SourcePath, bool), Result<&'a LinkedCode<'a>>>>,
}

impl<'a> Language<'a> {
    pub fn new(legacy: Evaluator<'a>) -> Self {
        let arena = legacy.code_bump;
        Self {
            legacy,
            closures: Published::empty(arena),
            thunk_dispatch: Published::empty(arena),
            lambda_dispatch: Published::empty(arena),
            literal_ids: RefCell::new(HashMap::new()),
            primops: RefCell::new(Vec::new()),
            #[cfg(brix_parallel)]
            primop_table: Published::empty(arena),
            primop_ids: RefCell::new(HashMap::new()),
            callbacks: RefCell::new(HashMap::new()),
            #[cfg(brix_parallel)]
            file_codes: RefCell::new(HashMap::new()),
        }
    }
    #[cfg(brix_parallel)]
    pub(crate) fn import_code(
        &self,
        path: &brix_store::SourcePath,
        must_be_attrs: bool,
    ) -> Result<&'a LinkedCode<'a>> {
        // This gate covers metadata loading only: never managed allocation,
        // forcing, or collection. Concurrent imports parse/link a file once.
        let _metadata = self.legacy.metadata_gate.lock();
        let key = (path.clone(), must_be_attrs);
        if let Some(result) = self.file_codes.borrow().get(&key) {
            return result.as_ref().copied().map_err(crate::error::snapshot);
        }
        let result = self.legacy.load_file_code(path, None, must_be_attrs);
        self.file_codes.borrow_mut().insert(key, result.clone());
        result.map_err(|error| crate::error::snapshot(&error))
    }

    pub(crate) fn register_callback(
        &self,
        name: &str,
        callback: Rc<crate::managed_api::Callback<'a>>,
    ) -> Result<()> {
        let name = name.strip_prefix("__").unwrap_or(name);
        let found = self.legacy.internal.borrow().get(name).copied();
        if !matches!(found.map(|cell| cell.get()), Some(Value::PrimOp(_))) {
            return Err(self.legacy.error(
                ErrorKind::Eval,
                format!("native callback '{name}' has no registered primop metadata"),
            ));
        }
        self.callbacks
            .borrow_mut()
            .insert(name.to_owned(), callback);
        Ok(())
    }

    pub fn legacy(&self) -> &Evaluator<'a> {
        &self.legacy
    }

    pub fn eval_str<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        src: &[u8],
        base_dir: impl AsRef<[u8]>,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.eval_source(machine, roots, src, base_dir, Origin::String)
    }

    pub fn eval_source<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        src: &[u8],
        base_dir: impl AsRef<[u8]>,
        origin: Origin,
    ) -> Result<ValueRoot<'r, 'h>> {
        let expr = self.legacy.parse_source(src, base_dir.as_ref(), &origin)?;
        let code = self.legacy.load(&expr, origin, src.to_vec(), None, false)?;
        let env = self.base_env(machine, roots)?;
        self.eval_code(machine, roots, code, env)
    }

    pub fn eval_internal<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        name: &str,
        source: &str,
    ) -> Result<ValueRoot<'r, 'h>> {
        let origin = Origin::Internal {
            name: name.to_owned(),
        };
        let expr = self.legacy.parse_source(source.as_bytes(), b"/", &origin)?;
        let code = self
            .legacy
            .load(&expr, origin, source.as_bytes().to_vec(), None, true)?;
        let env = self.base_env(machine, roots)?;
        self.eval_code(machine, roots, code, env)
    }

    pub fn eval_code<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.node(machine, roots, code, code.root, env)
    }

    fn node<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        env: EnvRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let eval = self.compiled(code, node, 0);
        // A thunk or lambda body owns one temporary root frame. Strict child
        // closures share it; each nested call/force and builtin loop maintains
        // its own bounded frame. No temporary roots survive this evaluation.
        roots.with_roots(|temporary| {
            eval(self, machine, temporary, env).map(|value| roots.copy(value))
        })
    }

    #[inline]
    pub fn force<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        machine.force(self, roots, cell, pos)
    }

    #[inline]
    pub fn force_cell<'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'_, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<()> {
        machine.force_cell(self, roots, cell, pos)
    }

    pub(crate) fn base_env<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
    ) -> Result<EnvRoot<'r, 'h>> {
        self.legacy.seal();
        if let Some(env) = &machine.base_env {
            return Ok(roots.copy(env.root()));
        }
        let mut importer = Importer {
            language: self,
            machine,
            roots,
            cells: HashMap::new(),
            envs: HashMap::new(),
        };
        let uninit = importer.cell(self.legacy.consts.uninit)?;
        let empty_list = importer.cell(self.legacy.consts.empty_list)?;
        let env = importer.env(self.legacy.base_env)?;
        importer.machine.uninit = Some(importer.machine.heap.persist(uninit));
        importer.machine.empty_list = Some(importer.machine.heap.persist(empty_list));
        importer.machine.base_env = Some(importer.machine.heap.persist(env));
        Ok(env)
    }

    fn literal_id(&self, code: &LinkedCode<'a>, node: NodeId) -> LiteralId {
        let global = code.node_base + node.0;
        let mut ids = self.literal_ids.borrow_mut();
        let next = ids.len();
        *ids.entry(global).or_insert_with(|| {
            // Unique literal IDs cannot outnumber the u32 global node space.
            LiteralId(u32::try_from(next).expect("literal ID fits global node space"))
        })
    }

    #[inline]
    fn literal<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        id: LiteralId,
    ) -> Result<ValueRoot<'r, 'h>> {
        if let Some(cell) = machine.literals.get(id.0 as usize).and_then(Option::as_ref) {
            return Ok(machine.heap.value(roots, cell.root()));
        }
        let cell = self.literal_cell(machine, roots, code, node, id)?;
        Ok(machine.heap.value(roots, cell))
    }

    #[inline]
    fn literal_cell<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        id: LiteralId,
    ) -> Result<CellRoot<'r, 'h>> {
        if let Some(value) = machine.literals.get(id.0 as usize).and_then(Option::as_ref) {
            return Ok(roots.copy(value.root()));
        }
        self.import_literal(machine, roots, code, node, id)
    }

    #[cold]
    #[inline(never)]
    fn import_literal<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        id: LiteralId,
    ) -> Result<CellRoot<'r, 'h>> {
        let global = code.node_base + node.0;
        let value = code.consts[node.0 as usize]
            .expect("literal has a constant")
            .get();
        let mut importer = Importer {
            language: self,
            machine,
            roots,
            cells: HashMap::new(),
            envs: HashMap::new(),
        };
        let value = importer.value(value)?;
        if matches!(code.node(node), Node::Path { .. }) {
            importer
                .machine
                .heap
                .set_path_literal(value, GlobalNode(global));
        }
        let cell = allocation(importer.machine.heap.cell(roots, value))?;
        let cache = &mut importer.machine.literals;
        let index = id.0 as usize;
        if cache.len() <= index {
            let len = allocation(
                index
                    .checked_add(1)
                    .ok_or(brix_gc::Error::AllocationTooLarge),
            )?;
            cache.resize_with(len, || None);
        }
        cache[index] = Some(importer.machine.heap.persist(cell));
        Ok(cell)
    }

    pub fn builtin<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        name: &str,
    ) -> Result<CellRoot<'r, 'h>> {
        let env = self.base_env(machine, roots)?;
        let old = self.legacy.builtin(name);
        if let Some(index) = self
            .legacy
            .base_env
            .slots()
            .iter()
            .position(|slot| std::ptr::eq(slot.get(), old))
        {
            return Ok(machine
                .heap
                .env_view(env)
                .get(index)
                .expect("builtin's global slot")
                .root(roots));
        }
        let mut importer = Importer {
            language: self,
            machine,
            roots,
            cells: HashMap::new(),
            envs: HashMap::new(),
        };
        importer.cell(old)
    }

    pub fn primop_name(&self, id: usize) -> &'a str {
        self.primop(id).name
    }

    #[cfg(not(brix_parallel))]
    #[inline]
    fn primop(&self, id: usize) -> Primop<'a> {
        self.primops.borrow()[id]
    }

    #[cfg(brix_parallel)]
    #[inline]
    fn primop(&self, id: usize) -> Primop<'a> {
        self.primop_table.get()[id]
    }

    /// Append under the table's write lock; an id is handed out only after
    /// the snapshot containing it is published.
    fn push_primop(&self, primops: &mut Vec<Primop<'a>>, primop: Primop<'a>) -> usize {
        let index = primops.len();
        primops.push(primop);
        #[cfg(brix_parallel)]
        self.primop_table
            .set(self.legacy.code_bump.alloc_slice_copy(primops));
        index
    }

    pub(crate) fn native_primop(&self, name: &'static str, arity: u8, add_trace: bool) -> usize {
        let mut primops = self.primops.borrow_mut();
        if let Some(index) = primops
            .iter()
            .position(|primop| primop.name == name && primop.arity == arity)
        {
            return index;
        }
        let primop = Primop {
            name,
            dispatch_name: name.strip_prefix("__").unwrap_or(name),
            handler: crate::managed_builtins::resolve(name),
            arity,
            add_trace,
            has_callback: self.callbacks.borrow().contains_key(name),
        };
        self.push_primop(&mut primops, primop)
    }

    pub(crate) fn auto_call<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        args: BindingsRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.legacy.with_stack(|| {
            let pos = match m.heap.read(fun) {
                ValueView::Lambda { node, .. } => { let (code, lambda) = self.legacy.lambda_def(node); self.legacy.src_pos(code, lambda.pos) },
                ValueView::Attrs(attrs) => attrs.pos(),
                _ => SrcPos::NONE,
            };
            let f = self.force(m, r, fun, pos)?;
            if let ValueView::Attrs(attrs) = m.heap.view(f) {
                if let Some(functor) = attrs.get(sym::FUNCTOR) {
                    let fv = functor.value.root(r);
                    let fv = self.force(m, r, fv, functor.pos)?;
                    let value = self.call_value(m, r, fv, &[fun], pos)?;
                    let cell = allocation(m.heap.cell(r, value))?;
                    return self.auto_call(m, r, cell, args);
                }
            }
            let ValueView::Lambda { node, .. } = m.heap.view(f) else { return Ok(f) };
            let (code, lambda) = self.legacy.lambda_def(node);
            let Some(formals) = &lambda.formals else { return Ok(f) };
            let mut attrs = Vec::new();
            if formals.ellipsis {
                attrs.extend(m.heap.bindings_view(args).iter().map(|attr| attr.root(r)));
            } else {
                for (_, formal) in formals.in_id_order() {
                    match m.heap.bindings_view(args).get(formal.name) {
                        Some(attr) => attrs.push(attr.root(r)),
                        None if formal.default.is_none() => return Err(self.legacy.error_at(ErrorKind::MissingArgument, msg!("cannot evaluate a function that has an argument without a value ('", self.legacy.symbol_ident(formal.name), "')\nNix attempted to evaluate a function as a top level expression; in\nthis case it must have its arguments supplied either by default\nvalues, or passed explicitly with '--arg' or '--argstr'. See\nhttps://nix.dev/manual/nix/stable/language/syntax.html#functions."), self.legacy.src_pos(code, formal.pos))),
                        None => {},
                    }
                }
            }
            let arg = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, SrcPos::NONE))?;
            let arg = m.heap.bindings_value(r, arg);
            let arg = allocation(m.heap.cell(r, arg))?;
            self.call_value(m, r, f, &[arg], pos)
        })
    }

    fn primop_id(&self, p: &'a crate::value::PrimOp<'a>) -> usize {
        let _metadata = self.legacy.metadata_gate.lock();
        let key = p as *const _ as usize;
        if let Some(&id) = self.primop_ids.borrow().get(&key) {
            return id;
        }
        let mut primops = self.primops.borrow_mut();
        let primop = Primop {
            name: p.name,
            dispatch_name: p.name.strip_prefix("__").unwrap_or(p.name),
            handler: crate::managed_builtins::resolve(p.name),
            arity: p.arity,
            add_trace: p.add_trace,
            has_callback: self.callbacks.borrow().contains_key(p.name),
        };
        let id = self.push_primop(&mut primops, primop);
        self.primop_ids.borrow_mut().insert(key, id);
        id
    }

    fn mk<F>(&self, function: F) -> Eval<'a>
    where
        F: for<'r, 'h, 'e> Fn(
                &Language<'a>,
                &mut Machine<'h>,
                &Roots<'r, 'h>,
                EnvRoot<'e, 'h>,
            ) -> Result<ValueRoot<'r, 'h>>
            + crate::runtime::ThreadSafe
            + 'a,
    {
        self.legacy.code_bump.alloc(function)
    }

    #[inline]
    fn compiled(&self, code: &'a LinkedCode<'a>, node: NodeId, depth: u32) -> Eval<'a> {
        let page = (code.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        if let Some(file) = self.closures.get().get(page) {
            if let Some(slot) = file.get().get(node.0 as usize) {
                if let Some(eval) = slot.get() {
                    return eval;
                }
            }
        }
        self.compile_uncached(code, node, depth)
    }

    #[inline]
    fn thunk_dispatch(&self, node: GlobalNode) -> Eval<'a> {
        let page = (node.0 >> crate::code::NODE_PAGE_BITS) as usize;
        let offset = (node.0 & ((1 << crate::code::NODE_PAGE_BITS) - 1)) as usize;
        if let Some(page) = self.thunk_dispatch.get().get(page) {
            if let Some(slot) = page.get().get(offset) {
                if let Some(eval) = slot.get() {
                    return eval;
                }
            }
        }
        self.prepare_thunk_dispatch(node)
    }

    #[cold]
    #[inline(never)]
    fn prepare_thunk_dispatch(&self, node: GlobalNode) -> Eval<'a> {
        let _metadata = self.legacy.metadata_gate.lock();
        let (code, local) = self.legacy.resolve_node(node);
        // Use the same lazy compilation path as before. A strict parent may
        // already have compiled this slot with its stack-growth wrapper.
        let eval = self.compiled(code, local, 0);
        let page_size = 1usize << crate::code::NODE_PAGE_BITS;
        let page_index = (node.0 >> crate::code::NODE_PAGE_BITS) as usize;
        let mut pages = self.thunk_dispatch.get();
        if page_index >= pages.len() {
            let previous = pages;
            pages = self.legacy.code_bump.alloc_slice_fill_with(
                (page_index + 1).next_power_of_two(),
                |index| {
                    Published::new(
                        previous.get(index).map_or(&[][..], Published::get),
                        self.legacy.code_bump,
                    )
                },
            );
            self.thunk_dispatch.set(pages);
        }
        // Compilation publishes the file's complete slot array before any
        // recursive compilation. Its arena allocation never moves, even when
        // either page table grows. Future strict compilation is visible here.
        let file_page = (code.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        let slots = self.closures.get()[file_page].get();
        let first = page_index * page_size - code.node_base as usize;
        let end = (first + page_size).min(slots.len());
        pages[page_index].set(&slots[first..end]);
        eval
    }

    #[inline]
    fn lambda_dispatch(&self, node: GlobalNode) -> &'a LambdaDispatch<'a> {
        let page = (node.0 >> crate::code::NODE_PAGE_BITS) as usize;
        let offset = (node.0 & ((1 << crate::code::NODE_PAGE_BITS) - 1)) as usize;
        if let Some(page) = self.lambda_dispatch.get().get(page) {
            if let Some(slot) = page.get().get(offset) {
                if let Some(dispatch) = slot.get() {
                    return dispatch;
                }
            }
        }
        self.prepare_lambda_dispatch(node)
    }

    #[cold]
    #[inline(never)]
    fn prepare_lambda_dispatch(&self, node: GlobalNode) -> &'a LambdaDispatch<'a> {
        let _metadata = self.legacy.metadata_gate.lock();
        let (code, definition) = self.legacy.lambda_def(node);
        let page_size = 1usize << crate::code::NODE_PAGE_BITS;
        let page_index = (node.0 >> crate::code::NODE_PAGE_BITS) as usize;
        let mut pages = self.lambda_dispatch.get();
        if page_index >= pages.len() {
            let previous = pages;
            pages = self.legacy.code_bump.alloc_slice_fill_with(
                (page_index + 1).next_power_of_two(),
                |index| {
                    Published::new(
                        previous.get(index).map_or(&[][..], Published::get),
                        self.legacy.code_bump,
                    )
                },
            );
            self.lambda_dispatch.set(pages);
        }
        let page = &pages[page_index];
        let mut slots = page.get();
        if slots.is_empty() {
            // Files begin on global-node page boundaries. Only their final
            // page can be partial, and no unused tail slots are allocated.
            let first = page_index * page_size - code.node_base as usize;
            let len = (code.nodes.len() - first).min(page_size);
            slots = self
                .legacy
                .code_bump
                .alloc_slice_fill_with(len, |_| Cell::new(None));
            page.set(slots);
        }
        let dispatch = self.legacy.code_bump.alloc(LambdaDispatch {
            code,
            definition,
            body: Published::empty(self.legacy.code_bump),
        });
        slots[node.0 as usize & (page_size - 1)].set(Some(dispatch));
        dispatch
    }

    // Publish each file's array before recursive compilation, so strict children
    // fill the same slots. Page-table growth copies only stable slice pointers;
    // previously returned slot references survive both growth and recursion.
    #[cold]
    #[inline(never)]
    fn compile_uncached(&self, code: &'a LinkedCode<'a>, node: NodeId, depth: u32) -> Eval<'a> {
        let _metadata = self.legacy.metadata_gate.lock();
        let page = (code.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        let mut pages = self.closures.get();
        if page >= pages.len() {
            let previous = pages;
            pages = self.legacy.code_bump.alloc_slice_fill_with(
                (page + 1).next_power_of_two(),
                |index| {
                    Published::new(
                        previous.get(index).map_or(&[][..], Published::get),
                        self.legacy.code_bump,
                    )
                },
            );
            self.closures.set(pages);
        }
        let file = &pages[page];
        let mut slots = file.get();
        if slots.is_empty() {
            slots = self
                .legacy
                .code_bump
                .alloc_slice_fill_with(code.nodes.len(), |_| {
                    Published::empty(self.legacy.code_bump)
                });
            file.set(slots);
        }
        let slot = &slots[node.0 as usize];
        // Another worker may have compiled this slot while we waited on the
        // metadata gate. Reuse its canonical closure and stack wrapper.
        if let Some(eval) = slot.get() {
            return eval;
        }
        let eval = self
            .legacy
            .with_stack(|| self.compile_node(code, node, depth));
        let eval: Eval<'a> = if depth > 0 && depth % 8 == 0 {
            self.mk(move |language, machine, roots, env| {
                language
                    .legacy
                    .with_stack(|| eval(language, machine, roots, env))
            })
        } else {
            eval
        };
        slot.set(Some(eval));
        eval
    }

    fn compile_node(&self, code: &'a LinkedCode<'a>, node: NodeId, depth: u32) -> Eval<'a> {
        let strict = |n| self.compiled(code, n, depth + 1);
        let sp = |p| self.legacy.src_pos(code, p);
        match *code.node(node) {
            Node::Int(value) => self.mk(move |_, _, roots, _| Ok(roots.int(value))),
            Node::Float(value) => self.mk(move |_, _, roots, _| Ok(roots.float(value))),
            Node::Str(_) | Node::Path { .. } => {
                let id = self.literal_id(code, node);
                self.mk(move |l, m, r, _| l.literal(m, r, code, node, id))
            }
            Node::Lambda(_) => self.mk(move |_, m, r, env| {
                Ok(m.heap.lambda(r, env, GlobalNode(code.node_base + node.0)))
            }),
            Node::Var {
                pos,
                var: VarRef::Env { level, displ },
                ..
            } => {
                let pos = sp(pos);
                match level {
                    0 => self.mk(move |l, m, r, env| l.force_env_var(m, r, env, 0, displ, pos)),
                    1 => self.mk(move |l, m, r, env| l.force_env_var(m, r, env, 1, displ, pos)),
                    _ => self.mk(move |l, m, r, env| l.force_env_var(m, r, env, level, displ, pos)),
                }
            }
            Node::Var { pos, name, var } => {
                let pos = sp(pos);
                self.mk(move |l, m, r, env| {
                    let cell = l.lookup(m, r, code, env, name, var, pos)?;
                    l.force(m, r, cell, pos)
                })
            }
            Node::InheritFrom { displ, .. } => self.mk(move |l, m, r, env| {
                let cell = m
                    .heap
                    .env_view(env)
                    .get(displ as usize)
                    .expect("inherit slot")
                    .root(r);
                l.force(m, r, cell, SrcPos::NONE)
            }),
            Node::Select {
                pos,
                subject,
                path,
                default,
            } => {
                let subject = strict(subject);
                let pos = sp(pos);
                match (code.attr_names_in(path), default) {
                    (&[AttrName::Static(name)], None) => self.mk(move |l, m, r, env| {
                        let value = subject(l, m, r, env)?;
                        let attr = match m.heap.view(value) {
                            ValueView::Attrs(set) => {
                                let attr = m.heap.lookup_attribute(set, name);
                                #[cfg(feature = "work-counters")]
                                m.heap.profile_attribute_lookup(
                                    l.legacy().stats(),
                                    set,
                                    name,
                                    attr.is_some(),
                                    0,
                                );
                                attr
                            }
                            _ => None,
                        };
                        let Some(attr) = attr else {
                            return l.select(m, r, code, env, value, pos, path, None);
                        };
                        let attribute_pos = attr.pos;
                        if m.heap.is_whnf_view(attr.value) {
                            Ok(m.heap.value_from_view(r, attr.value))
                        } else {
                            let cell = attr.value.root(r);
                            l.force(m, r, cell, attribute_pos.or(pos))
                                .inspect_err(|error| {
                                    l.trace_selected_attr(error, attribute_pos, name);
                                })
                        }
                    }),
                    _ => self.mk(move |l, m, r, env| {
                        let value = subject(l, m, r, env)?;
                        l.select(m, r, code, env, value, pos, path, default)
                    }),
                }
            }
            Node::HasAttr { subject, path } => {
                let subject = strict(subject);
                let pos = sp(code.pos_of(node));
                self.mk(move |l, m, r, env| {
                    let value = subject(l, m, r, env)?;
                    l.has_attr(m, r, code, env, value, pos, path)
                })
            }
            Node::Attrs(id) => self.compile_attrs(code, id),
            Node::List { elems } if elems.is_empty() => self.mk(|_, machine, roots, _| {
                let cell = machine
                    .empty_list
                    .as_ref()
                    .expect("base environment initialized");
                Ok(machine.heap.value(roots, cell.root()))
            }),
            Node::List { elems } => {
                let plans: Vec<_> = code
                    .nodes_in(elems)
                    .iter()
                    .map(|&n| self.lazy(code, n))
                    .collect();
                let plans = self.legacy.code_bump.alloc_slice_copy(&plans);
                self.mk(move |l, m, r, env| {
                    let cells = plans
                        .iter()
                        .map(|p| l.lazy_cell(m, r, code, env, *p))
                        .collect::<Result<Vec<_>>>()?;
                    allocation(m.heap.list(r, &cells))
                })
            }
            Node::Call { pos, fun, args } => {
                let fun = strict(fun);
                let pos = sp(pos);
                let plans: Vec<_> = code
                    .nodes_in(args)
                    .iter()
                    .map(|&n| self.lazy(code, n))
                    .collect();
                let plans = self.legacy.code_bump.alloc_slice_copy(&plans);
                match plans {
                    [a] => {
                        let a = *a;
                        self.mk(move |l, m, r, env| {
                            let fun = fun(l, m, r, env)?;
                            let arg = l.lazy_cell(m, r, code, env, a)?;
                            l.call_value(m, r, fun, &[arg], pos)
                        })
                    }
                    [a, b] => {
                        let (a, b) = (*a, *b);
                        self.mk(move |l, m, r, env| {
                            let fun = fun(l, m, r, env)?;
                            let a = l.lazy_cell(m, r, code, env, a)?;
                            let b = l.lazy_cell(m, r, code, env, b)?;
                            l.call_value(m, r, fun, &[a, b], pos)
                        })
                    }
                    _ => self.mk(move |l, m, r, env| {
                        let fun = fun(l, m, r, env)?;
                        let args = plans
                            .iter()
                            .map(|p| l.lazy_cell(m, r, code, env, *p))
                            .collect::<Result<smallvec::SmallVec<[_; 4]>>>()?;
                        l.call_value(m, r, fun, &args, pos)
                    }),
                }
            }
            Node::Let { attrs, body } => {
                let body = strict(body);
                let set = code.attr_set(attrs);
                let plans: Vec<_> = set
                    .in_id_order()
                    .map(|(i, d)| (i, d.kind, self.lazy(code, d.expr)))
                    .collect();
                let plans = self.legacy.code_bump.alloc_slice_copy(&plans);
                self.mk(move |l, m, r, env| {
                    let env = r.copy(env);
                    let frame = l.frame(m, r, Some(env), set.attrs.len())?;
                    let inherited = l.inherit_env(m, r, code, set, frame)?;
                    for &(i, kind, plan) in plans.iter() {
                        let value =
                            l.lazy_cell(m, r, code, kind.choose(frame, env, inherited), plan)?;
                        m.heap.env_set(frame, i, value);
                    }
                    body(l, m, r, frame)
                })
            }
            Node::With { attrs, body, .. } => {
                let plan = self.lazy(code, attrs);
                let body = strict(body);
                self.mk(move |l, m, r, env| {
                    let cell = l.lazy_cell(m, r, code, env, plan)?;
                    let frame = allocation(m.heap.env(r, Some(env), &[cell]))?;
                    body(l, m, r, frame)
                })
            }
            Node::If {
                pos,
                cond,
                then_,
                else_,
            } => {
                let pos = sp(pos);
                let cond = strict(cond);
                let then_ = strict(then_);
                let else_ = strict(else_);
                self.mk(move |l, m, r, env| {
                    let result = cond(l, m, r, env)
                        .map_err(|e| l.traced(e, pos, "while evaluating a branch condition"))?;
                    if l.boolean(m, result, pos, "while evaluating a branch condition")? {
                        then_(l, m, r, env)
                    } else {
                        else_(l, m, r, env)
                    }
                })
            }
            Node::Not(node) => {
                let pos = sp(code.pos_of(node));
                let operand = strict(node);
                self.mk(move |l, m, r, env| {
                    let value = operand(l, m, r, env)
                        .map_err(|e| l.traced(e, pos, "in the argument of the not operator"))?;
                    Ok(r.bool(!l.boolean(m, value, pos, "in the argument of the not operator")?))
                })
            }
            Node::BinOp { pos, op, lhs, rhs } => {
                self.compile_binop(sp(pos), op, strict(lhs), strict(rhs))
            }
            Node::ConcatStrings {
                pos,
                force_string,
                parts,
            } => {
                let pos = sp(pos);
                let parts: Vec<_> = code
                    .parts_in(parts)
                    .iter()
                    .map(|&(p, n)| (sp(p), strict(n)))
                    .collect();
                let parts = self.legacy.code_bump.alloc_slice_copy(&parts);
                self.mk(move |l, m, r, env| l.concat(m, r, env, pos, force_string, parts))
            }
            Node::CurPos(pos) => {
                let pos = sp(pos);
                self.mk(move |l, m, r, _| l.position_value(m, r, pos))
            }
            Node::Assert { pos, cond, body } => {
                let pos = sp(pos);
                let condition = strict(cond);
                let body = strict(body);
                self.mk(move |l, m, r, env| {
                    let value = condition(l, m, r, env).map_err(|e| {
                        l.traced(e, pos, "in the condition of the assert statement")
                    })?;
                    if !l.boolean(m, value, pos, "in the condition of the assert statement")? {
                        return Err(l.assert_failure(m, r, code, env, cond, pos));
                    }
                    body(l, m, r, env)
                })
            }
        }
    }

    fn lazy(&self, code: &LinkedCode<'a>, node: NodeId) -> Lazy {
        match code.node(node) {
            Node::Int(_) | Node::Float(_) | Node::Str(_) | Node::Path { .. } => Lazy::Literal {
                node,
                id: self.literal_id(code, node),
            },
            Node::Var {
                var: VarRef::Env { level, displ },
                ..
            } => Lazy::Variable {
                node,
                level: *level,
                displ: *displ,
            },
            Node::List { elems } if elems.is_empty() => Lazy::EmptyList,
            _ => Lazy::Thunk(node),
        }
    }

    // Always inline this shared force path so level-0/1 compiled closures see
    // a constant chain length and eliminate the loop. Borrow evaluated cells
    // directly; only suspended cells need a cell root before force can collect.
    #[inline(always)]
    fn force_env_var<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        level: u32,
        displ: u32,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        let mut view = machine.heap.env_view(env);
        for _ in 0..level {
            view = view.up().expect("resolved environment level");
        }
        let cell = view.get(displ as usize).expect("resolved variable slot");
        if machine.heap.is_whnf_view(cell) {
            Ok(machine.heap.value_from_view(roots, cell))
        } else {
            let cell = cell.root(roots);
            self.force(machine, roots, cell, pos)
        }
    }

    fn env_cell<'m, 'h>(
        &self,
        machine: &'m Machine<'h>,
        env: EnvRoot<'_, 'h>,
        level: u32,
        displ: u32,
    ) -> crate::managed_heap::CellView<'m, 'h> {
        let mut view = machine.heap.env_view(env);
        for _ in 0..level {
            view = view.up().expect("resolved environment level");
        }
        view.get(displ as usize).expect("resolved variable slot")
    }

    fn up<'r, 'h>(
        &self,
        machine: &Machine<'h>,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        level: u32,
    ) -> EnvRoot<'r, 'h> {
        let mut view = machine.heap.env_view(env);
        for _ in 0..level {
            view = view.up().expect("resolved environment level");
        }
        view.root(roots)
    }

    #[inline(always)]
    fn lazy_cell<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        plan: Lazy,
    ) -> Result<CellRoot<'r, 'h>> {
        let node = match plan {
            Lazy::EmptyList => {
                return Ok(roots.copy(
                    machine
                        .empty_list
                        .as_ref()
                        .expect("base environment initialized")
                        .root(),
                ));
            }
            Lazy::Literal { node, id } => return self.literal_cell(machine, roots, code, node, id),
            Lazy::Variable { node, level, displ } => {
                let cell = self.env_cell(machine, env, level, displ).root(roots);
                if !machine.heap.same_cell(
                    cell,
                    machine
                        .uninit
                        .as_ref()
                        .expect("base environment initialized")
                        .root(),
                ) {
                    return Ok(cell);
                }
                node
            }
            Lazy::Thunk(node) => node,
        };
        allocation(
            machine
                .heap
                .thunk_cell(roots, env, GlobalNode(code.node_base + node.0)),
        )
    }

    fn frame<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        up: Option<EnvRoot<'_, 'h>>,
        len: usize,
    ) -> Result<EnvRoot<'r, 'h>> {
        let uninit = machine
            .uninit
            .as_ref()
            .expect("base environment initialized")
            .root();
        allocation(machine.heap.env_filled(roots, up, len, uninit))
    }

    fn inherit_env<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        set: &'a crate::code::AttrSet<'a>,
        up: EnvRoot<'_, 'h>,
    ) -> Result<Option<EnvRoot<'r, 'h>>> {
        if set.inherit_from.is_empty() {
            return Ok(None);
        }
        let frame = self.frame(machine, roots, Some(up), set.inherit_from.len())?;
        for (i, &node) in set.inherit_from.iter().enumerate() {
            let cell = self.lazy_cell(machine, roots, code, up, self.lazy(code, node))?;
            machine.heap.env_set(frame, i, cell);
        }
        Ok(Some(frame))
    }

    fn traced(&self, error: crate::Error, pos: SrcPos, hint: &str) -> crate::Error {
        self.legacy.add_trace(&error, pos, hint);
        error
    }

    fn boolean<'h>(
        &self,
        machine: &Machine<'h>,
        value: ValueRoot<'_, 'h>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<bool> {
        match machine.heap.view(value) {
            ValueView::Bool(value) => Ok(value),
            _ => Err(self.traced(
                crate::managed_builtins::type_error(self, machine, value, "a Boolean", pos),
                pos,
                ctx,
            )),
        }
    }

    fn expect_attrs<'r, 'h>(
        &self,
        machine: &Machine<'h>,
        roots: &Roots<'r, 'h>,
        value: ValueRoot<'_, 'h>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<BindingsRoot<'r, 'h>> {
        match machine.heap.view(value) {
            ValueView::Attrs(set) => Ok(set.root(roots)),
            _ => Err(self.traced(
                crate::managed_builtins::type_error(self, machine, value, "a set", SrcPos::NONE),
                pos,
                ctx,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        name: Symbol,
        var: VarRef,
        pos: SrcPos,
    ) -> Result<CellRoot<'r, 'h>> {
        let (mut env, mut with) = match var {
            VarRef::Env { level, displ } => {
                return Ok(self.env_cell(machine, env, level, displ).root(roots));
            }
            VarRef::With { level, with } => (self.up(machine, roots, env, level), with),
        };
        loop {
            let Node::With {
                pos: wpos,
                prev_with,
                parent_with,
                ..
            } = *code.node(with)
            else {
                unreachable!("with reference")
            };
            let cell = machine
                .heap
                .env_view(env)
                .get(0)
                .expect("with set")
                .root(roots);
            let set = crate::managed_builtins::force_attrs(
                self,
                machine,
                roots,
                cell,
                self.legacy.src_pos(code, wpos),
                "while evaluating the first subexpression of a with expression",
            )?;
            if let Some(attr) = machine.heap.bindings_view(set).get(name) {
                return Ok(attr.value.root(roots));
            }
            match parent_with {
                Some(parent) => {
                    env = self.up(machine, roots, env, prev_with);
                    with = parent;
                }
                None => {
                    return Err(self.legacy.error_at(
                        ErrorKind::UndefinedVar,
                        msg!("undefined variable '", self.legacy.symbol_ident(name), "'"),
                        pos,
                    ));
                }
            }
        }
    }

    fn attr_name<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        name: AttrName,
    ) -> Result<Symbol> {
        match name {
            AttrName::Static(name) => Ok(name),
            AttrName::Dynamic(node) => {
                let v = self.node(m, r, code, node, env)?;
                let cell = allocation(m.heap.cell(r, v))?;
                let text = crate::managed_builtins::force_string_no_ctx_value(
                    self,
                    m,
                    r,
                    cell,
                    self.legacy.src_pos(code, code.pos_of(node)),
                    "while evaluating an attribute name",
                )?;
                Ok(self
                    .legacy
                    .intern(crate::managed_builtins_strings::bytes(m, text)))
            }
        }
    }

    #[cold]
    fn trace_selected_attr(&self, error: &crate::Error, pos: SrcPos, name: Symbol) {
        if pos.is_some() && !self.legacy.is_internal_pos(pos) {
            self.legacy.add_trace(
                error,
                pos,
                msg!(
                    "while evaluating the attribute '",
                    self.legacy.symbol_ident(name),
                    "'"
                ),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn select<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        mut value: ValueRoot<'r, 'h>,
        pos: SrcPos,
        path: crate::ir::Range,
        default: Option<NodeId>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let mut attribute_pos = SrcPos::NONE;
        let result = (|| {
            let mut pending = None;
            for &component in code.attr_names_in(path) {
                if let Some(cell) = pending.take() {
                    value = self.force(m, r, cell, pos)?;
                }
                let name = self.attr_name(m, r, code, env, component)?;
                let attr = match m.heap.view(value) {
                    ValueView::Attrs(set) => {
                        let attr = m.heap.lookup_attribute(set, name);
                        #[cfg(feature = "work-counters")]
                        m.heap.profile_attribute_lookup(
                            self.legacy().stats(),
                            set,
                            name,
                            attr.is_some(),
                            1,
                        );
                        attr.map(|attr| attr.root(r))
                    }
                    _ if default.is_some() => None,
                    _ => {
                        self.expect_attrs(m, r, value, pos, "while selecting an attribute")?;
                        unreachable!()
                    }
                };
                match attr {
                    Some(attr) => {
                        attribute_pos = attr.pos;
                        pending = Some(attr.value);
                    }
                    None => {
                        if let Some(default) = default {
                            return self.node(m, r, code, default, env);
                        }
                        let set =
                            self.expect_attrs(m, r, value, pos, "while selecting an attribute")?;
                        let error = self.legacy.error_at(
                            ErrorKind::Eval,
                            msg!("attribute '", self.legacy.symbol_ident(name), "' missing"),
                            pos,
                        );
                        error.set_suggestions(crate::error::best_suggestions_bytes(
                            m.heap
                                .bindings_view(set)
                                .iter()
                                .map(|a| self.legacy.symbol_name(a.name)),
                            self.legacy.symbol_name(name),
                        ));
                        return Err(error);
                    }
                }
            }
            match pending {
                Some(cell) => self.force(m, r, cell, attribute_pos.or(pos)),
                None => Ok(value),
            }
        })();
        result.inspect_err(|error| {
            if attribute_pos.is_some() && !self.legacy.is_internal_pos(attribute_pos) {
                let mut shown = Vec::new();
                for (i, &name) in code.attr_names_in(path).iter().enumerate() {
                    if i > 0 {
                        shown.push(b'.');
                    }
                    match self.attr_name(m, r, code, env, name) {
                        Ok(name) => shown.extend(self.legacy.symbol_ident(name)),
                        Err(_) => {
                            if let AttrName::Dynamic(node) = name {
                                shown.extend(msg!(
                                    "\"${",
                                    crate::show::show_node(&self.legacy, code, node),
                                    "}\""
                                ));
                            }
                        }
                    }
                }
                self.legacy.add_trace(
                    error,
                    attribute_pos,
                    msg!("while evaluating the attribute '", shown, "'"),
                );
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn has_attr<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        mut value: ValueRoot<'r, 'h>,
        pos: SrcPos,
        path: crate::ir::Range,
    ) -> Result<ValueRoot<'r, 'h>> {
        let names = code.attr_names_in(path);
        for (i, &name) in names.iter().enumerate() {
            let name = self.attr_name(m, r, code, env, name)?;
            let attr = match m.heap.view(value) {
                ValueView::Attrs(set) => {
                    let attr = set.get(name);
                    #[cfg(feature = "work-counters")]
                    m.heap.profile_attribute_lookup(
                        self.legacy().stats(),
                        set,
                        name,
                        attr.is_some(),
                        2,
                    );
                    attr.map(|attr| attr.root(r))
                }
                _ => None,
            };
            let Some(attr) = attr else {
                return Ok(r.bool(false));
            };
            if i + 1 < names.len() {
                value = self.force(m, r, attr.value, pos)?;
            }
        }
        Ok(r.bool(true))
    }

    fn compile_attrs(&self, code: &'a LinkedCode<'a>, id: crate::ir::AttrsId) -> Eval<'a> {
        let set = code.attr_set(id);
        if !set.dynamic_attrs.is_empty() || (set.recursive && set.find(sym::OVERRIDES).is_some()) {
            return self.mk(move |l, m, r, env| l.attrs(m, r, code, env, id));
        }

        // Linking fixes the symbol order, source positions and lazy plans.
        // Retain that work in permanent code, instead of decoding and sorting
        // the same definitions on each invocation of a function body.
        let defs: Vec<_> = set
            .in_id_order()
            .map(|(slot, def)| {
                (
                    slot,
                    def.kind,
                    self.lazy(code, def.expr),
                    def.name,
                    self.legacy.src_pos(code, def.pos),
                )
            })
            .collect();
        if !set.recursive
            && set.inherit_from.is_empty()
            && (1..=crate::bindings::FLAT_MAX).contains(&defs.len())
            && defs.iter().all(|(_, kind, plan, _, _)| {
                *kind == AttrKind::Plain && matches!(plan, Lazy::Thunk(_))
            })
        {
            let attrs: Vec<_> = defs
                .iter()
                .map(|&(_, _, plan, name, pos)| {
                    let Lazy::Thunk(node) = plan else {
                        unreachable!("checked thunk plan")
                    };
                    ThunkAttr {
                        name,
                        pos,
                        node: GlobalNode(code.node_base + node.0),
                    }
                })
                .collect();
            let attrs = self.legacy.code_bump.alloc_slice_copy(&attrs);
            let plan = self.legacy.code_bump.alloc(
                FlatThunkPlan::new(self.legacy.src_pos(code, set.pos), attrs)
                    .expect("linked attributes have unique sorted names and bounded layout"),
            );
            return self.mk(move |_l, m, r, env| {
                let bindings = allocation(m.heap.flat_thunk_attrs(r, env, plan))?;
                Ok(m.heap.bindings_value(r, bindings))
            });
        }
        if !set.recursive
            && set.inherit_from.is_empty()
            && (1..=crate::bindings::FLAT_MAX).contains(&defs.len())
            && defs
                .iter()
                .all(|(_, kind, _, _, _)| *kind != AttrKind::InheritedFrom)
        {
            let mut literals = Vec::new();
            let attrs: Vec<_> = defs
                .iter()
                .map(|&(_, _, plan, name, pos)| {
                    let source = match plan {
                        Lazy::Thunk(node) => {
                            LazyAttrSource::Thunk(GlobalNode(code.node_base + node.0))
                        }
                        Lazy::Variable { node, level, displ } => LazyAttrSource::Variable {
                            level,
                            displ,
                            fallback: GlobalNode(code.node_base + node.0),
                        },
                        Lazy::Literal { node, id } => {
                            literals.push((node, id));
                            LazyAttrSource::Literal(id.0)
                        }
                        Lazy::EmptyList => LazyAttrSource::EmptyList,
                    };
                    LazyAttr { name, pos, source }
                })
                .collect();
            let attrs = self.legacy.code_bump.alloc_slice_copy(&attrs);
            let literals = self.legacy.code_bump.alloc_slice_copy(&literals);
            let plan = self.legacy.code_bump.alloc(
                FlatLazyPlan::new(self.legacy.src_pos(code, set.pos), attrs)
                    .expect("linked attributes have unique sorted names and bounded layout"),
            );
            return self.mk(move |l, m, r, env| {
                // Only cold missing literals allocate here, in the same symbol
                // order as ordinary lazy initialization. Importing a literal
                // never evaluates an expression; warm leases need no new roots.
                for &(node, id) in literals.iter() {
                    if m.literals
                        .get(id.0 as usize)
                        .and_then(Option::as_ref)
                        .is_none()
                    {
                        l.import_literal(m, r, code, node, id)?;
                    }
                }
                let bindings = allocation(
                    m.heap.flat_lazy_attrs(
                        r,
                        env,
                        plan,
                        &m.literals,
                        m.empty_list
                            .as_ref()
                            .expect("base environment initialized")
                            .root(),
                        m.uninit
                            .as_ref()
                            .expect("base environment initialized")
                            .root(),
                    ),
                )?;
                Ok(m.heap.bindings_value(r, bindings))
            });
        }
        let defs = self.legacy.code_bump.alloc_slice_copy(&defs);
        let inherit: Vec<_> = set
            .inherit_from
            .iter()
            .map(|&node| self.lazy(code, node))
            .collect();
        let inherit = self.legacy.code_bump.alloc_slice_copy(&inherit);
        let pos = if defs.is_empty() {
            SrcPos::NONE
        } else {
            self.legacy.src_pos(code, set.pos)
        };
        let recursive = set.recursive;
        self.mk(move |l, m, r, env| {
            let env = r.copy(env);
            let frame = if recursive {
                l.frame(m, r, Some(env), defs.len())?
            } else {
                env
            };
            let inherited = if inherit.is_empty() {
                None
            } else {
                let inherited = l.frame(m, r, Some(frame), inherit.len())?;
                for (slot, &plan) in inherit.iter().enumerate() {
                    let value = l.lazy_cell(m, r, code, frame, plan)?;
                    m.heap.env_set(inherited, slot, value);
                }
                Some(inherited)
            };
            let mut attrs: smallvec::SmallVec<[_; 16]> =
                smallvec::SmallVec::with_capacity(defs.len());
            // Preserve Nix's symbol-order initialization of recursive slots;
            // earlier slots can already be shared by subsequent lazy values.
            for &(slot, kind, plan, name, pos) in defs.iter() {
                let value = l.lazy_cell(m, r, code, kind.choose(frame, env, inherited), plan)?;
                if recursive {
                    m.heap.env_set(frame, slot, value);
                }
                attrs.push(RootedAttr { name, pos, value });
            }
            let bindings = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, pos))?;
            Ok(m.heap.bindings_value(r, bindings))
        })
    }

    fn attrs<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        id: crate::ir::AttrsId,
    ) -> Result<ValueRoot<'r, 'h>> {
        let env = r.copy(env);
        let set = code.attr_set(id);
        let pos = self.legacy.src_pos(code, set.pos);
        let frame = if set.recursive {
            self.frame(m, r, Some(env), set.attrs.len())?
        } else {
            r.copy(env)
        };
        let inherited = self.inherit_env(m, r, code, set, frame)?;
        let overrides = set.recursive.then(|| set.find(sym::OVERRIDES)).flatten();
        let mut attrs = Vec::with_capacity(set.attrs.len() + set.dynamic_attrs.len());
        for (i, def) in set.in_id_order() {
            let environment = def.kind.choose(frame, env, inherited);
            let plan = if overrides.is_some() && def.kind != AttrKind::Inherited {
                Lazy::Thunk(def.expr)
            } else {
                self.lazy(code, def.expr)
            };
            let value = self.lazy_cell(m, r, code, environment, plan)?;
            if set.recursive {
                m.heap.env_set(frame, i, value);
            }
            attrs.push(RootedAttr {
                name: def.name,
                pos: self.legacy.src_pos(code, def.pos),
                value,
            });
        }
        if let Some(overrides) = overrides {
            let cell = m
                .heap
                .env_view(frame)
                .get(overrides.displ as usize)
                .expect("overrides slot")
                .root(r);
            let value = self.force(m, r, cell, SrcPos::NONE)?;
            let where_ = match m.heap.view(value) {
                ValueView::Attrs(set) => set.pos(),
                ValueView::Lambda { node, .. } => {
                    let (code, lam) = self.legacy.lambda_def(node);
                    self.legacy.src_pos(code, lam.pos)
                }
                _ => SrcPos::NONE,
            };
            let set_overrides = self.expect_attrs(
                m,
                r,
                value,
                where_,
                "while evaluating the `__overrides` attribute",
            )?;
            let overrides: Vec<_> = m
                .heap
                .bindings_view(set_overrides)
                .iter()
                .map(|a| a.root(r))
                .collect();
            for attr in overrides {
                if let Some(def) = set.find(attr.name) {
                    let index = attrs
                        .iter()
                        .position(|a| a.name == attr.name)
                        .expect("static attribute");
                    attrs[index] = attr;
                    m.heap.env_set(frame, def.displ as usize, attr.value);
                } else {
                    attrs.push(attr);
                }
            }
        }
        for dynamic in set.dynamic_attrs {
            let pos = self.legacy.src_pos(code, dynamic.pos);
            let value = self.node(m, r, code, dynamic.name, frame)?;
            if matches!(m.heap.view(value), ValueView::Null) {
                continue;
            }
            let cell = allocation(m.heap.cell(r, value))?;
            let name = crate::managed_builtins::force_string_no_ctx_value(
                self,
                m,
                r,
                cell,
                pos,
                "while evaluating the name of a dynamic attribute",
            )?;
            let name = self
                .legacy
                .intern(crate::managed_builtins_strings::bytes(m, name));
            if let Some(previous) = attrs.iter().find(|a| a.name == name) {
                return Err(self.legacy.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "dynamic attribute '",
                        self.legacy.symbol_ident(name),
                        "' already defined at ",
                        self.legacy
                            .position(previous.pos)
                            .map(|p| p.display())
                            .unwrap_or_default()
                    ),
                    pos,
                ));
            }
            let value = self.lazy_cell(m, r, code, frame, self.lazy(code, dynamic.value))?;
            attrs.push(RootedAttr { name, pos, value });
        }
        attrs.sort_unstable_by_key(|a| a.name);
        let bindings_pos = if attrs.is_empty() { SrcPos::NONE } else { pos };
        let bindings = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, bindings_pos))?;
        Ok(m.heap.bindings_value(r, bindings))
    }

    fn compile_binop(&self, pos: SrcPos, op: BinOp, left: Eval<'a>, right: Eval<'a>) -> Eval<'a> {
        match op {
            BinOp::Eq | BinOp::NEq => self.mk(move |l, m, r, env| {
                let a = left(l, m, r, env)?;
                let b = right(l, m, r, env)?;
                let same = crate::managed_builtins::equal_values(l, m, r, a, b, SrcPos::NONE)?;
                Ok(r.bool(if op == BinOp::Eq { same } else { !same }))
            }),
            BinOp::And | BinOp::Or | BinOp::Impl => {
                let (left_ctx, right_ctx) = match op {
                    BinOp::And => (
                        "in the left operand of the AND (&&) operator",
                        "in the right operand of the AND (&&) operator",
                    ),
                    BinOp::Or => (
                        "in the left operand of the OR (||) operator",
                        "in the right operand of the OR (||) operator",
                    ),
                    _ => (
                        "in the left operand of the IMPL (->) operator",
                        "in the right operand of the IMPL (->) operator",
                    ),
                };
                self.mk(move |l, m, r, env| {
                    let value = left(l, m, r, env).map_err(|e| l.traced(e, pos, left_ctx))?;
                    let lhs = l.boolean(m, value, pos, left_ctx)?;
                    if match op {
                        BinOp::Or => lhs,
                        _ => !lhs,
                    } {
                        return Ok(r.bool(op != BinOp::And));
                    }
                    let value = right(l, m, r, env).map_err(|e| l.traced(e, pos, right_ctx))?;
                    Ok(r.bool(l.boolean(m, value, pos, right_ctx)?))
                })
            }
            BinOp::Update => self.mk(move |l, m, r, env| {
                let lhs = left(l, m, r, env).map_err(|e| {
                    l.traced(e, pos, "in the left operand of the update (//) operator")
                })?;
                let lhs = l.expect_attrs(
                    m,
                    r,
                    lhs,
                    pos,
                    "in the left operand of the update (//) operator",
                )?;
                let rhs = right(l, m, r, env).map_err(|e| {
                    l.traced(e, pos, "in the right operand of the update (//) operator")
                })?;
                let rhs = l.expect_attrs(
                    m,
                    r,
                    rhs,
                    pos,
                    "in the right operand of the update (//) operator",
                )?;
                let set = allocation(bindings::update(&mut m.heap, r, lhs, rhs))?;
                Ok(m.heap.bindings_value(r, set))
            }),
            BinOp::ConcatLists => self.mk(move |l, m, r, env| {
                let lhs = left(l, m, r, env)?;
                let rhs = right(l, m, r, env)?;
                let a = allocation(m.heap.cell(r, lhs))?;
                let b = allocation(m.heap.cell(r, rhs))?;
                let a = crate::managed_builtins::force_list(
                    l,
                    m,
                    r,
                    a,
                    pos,
                    "while evaluating one of the elements to concatenate",
                )?;
                let b = crate::managed_builtins::force_list(
                    l,
                    m,
                    r,
                    b,
                    pos,
                    "while evaluating one of the elements to concatenate",
                )?;
                allocation(m.heap.concat_lists(r, a, b))
            }),
        }
    }

    fn concat<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        pos: SrcPos,
        force_string: bool,
        parts: &[(SrcPos, Eval<'a>)],
    ) -> Result<ValueRoot<'r, 'h>> {
        use crate::managed_builtins_strings::{self as strings, OwnedContext};
        #[derive(PartialEq)]
        enum Kind {
            Int,
            Float,
            Path,
            String,
        }
        let mut kind = Kind::String;
        let mut int = 0_i64;
        let mut float = 0_f64;
        let mut first = !force_string;
        let mut bytes = Vec::new(); // Path concatenation keeps its owned canonicalization buffer.
        let mut string_parts: smallvec::SmallVec<[_; 8]> = smallvec::SmallVec::new();
        let mut string_length = 0;
        let mut segments = strings::ContextSegments::new();
        let mut context = Vec::new();
        for (part_pos, eval) in parts {
            let value = eval(self, m, r, env)?;
            // A lone interpolated string preserves bytes and every context
            // element verbatim; copying into temporary buffers serves no purpose.
            if parts.len() == 1 && matches!(m.heap.view(value), ValueView::String(_)) {
                return Ok(value);
            }
            if first {
                kind = match m.heap.view(value) {
                    ValueView::Int(_) => Kind::Int,
                    ValueView::Float(_) => Kind::Float,
                    ValueView::Path(_) => Kind::Path,
                    _ => Kind::String,
                };
            }
            if kind == Kind::Int {
                match m.heap.view(value) {
                    ValueView::Int(i) => {
                        int = int.checked_add(i).ok_or_else(|| {
                            self.legacy.error_at(
                                ErrorKind::Eval,
                                format!("integer overflow in adding {int} + {i}"),
                                *part_pos,
                            )
                        })?
                    }
                    ValueView::Float(f) => {
                        kind = Kind::Float;
                        float = int as f64 + f;
                    }
                    v => {
                        return Err(self.legacy.error_at(
                            ErrorKind::Eval,
                            format!(
                                "cannot add {} to an integer",
                                crate::managed_builtins::show_type(v)
                            ),
                            *part_pos,
                        ));
                    }
                }
            } else if kind == Kind::Float {
                match m.heap.view(value) {
                    ValueView::Int(i) => float += i as f64,
                    ValueView::Float(f) => float += f,
                    v => {
                        return Err(self.legacy.error_at(
                            ErrorKind::Eval,
                            format!(
                                "cannot add {} to a float",
                                crate::managed_builtins::show_type(v)
                            ),
                            *part_pos,
                        ));
                    }
                }
            } else {
                match m.heap.view(value) {
                    ValueView::Path(path) if first => bytes.extend_from_slice(path.bytes()),
                    ValueView::String(_) => {
                        if kind == Kind::String {
                            string_length = strings::append_context_segment(
                                m,
                                value,
                                string_length,
                                &mut segments,
                            )?;
                            string_parts.push(value);
                        } else {
                            strings::append_snapshot(m, value, &mut bytes, &mut context)?;
                        }
                    }
                    _ => {
                        let cell = allocation(m.heap.cell(r, value))?;
                        let value = crate::managed_builtins::coerce_with_context(
                            self,
                            m,
                            r,
                            cell,
                            *part_pos,
                            crate::Coercion {
                                more: false,
                                copy_to_store: kind == Kind::String,
                                canonicalize: !first,
                            },
                            "while evaluating a path segment",
                        )?;
                        if kind == Kind::String {
                            string_length = strings::append_context_segment(
                                m,
                                value,
                                string_length,
                                &mut segments,
                            )?;
                            string_parts.push(value);
                        } else {
                            strings::append_snapshot(m, value, &mut bytes, &mut context)?;
                        };
                    }
                }
            }
            first = false;
        }
        match kind {
            Kind::Int => Ok(r.int(int)),
            Kind::Float => Ok(r.float(float)),
            Kind::Path => {
                if context.iter().any(OwnedContext::is_real) {
                    return Err(self.legacy.error_at(
                        ErrorKind::Eval,
                        "a string that refers to a store path cannot be appended to a path",
                        pos,
                    ));
                }
                let hashes: Vec<_> = context
                    .iter()
                    .filter_map(|c| match c {
                        OwnedContext::LazyHash { offset, tree } => {
                            Some(crate::lazy_tree::LazyHash {
                                offset: *offset as usize,
                                tree: *tree,
                            })
                        }
                        _ => None,
                    })
                    .collect();
                if !hashes.is_empty() {
                    bytes = self.legacy.concat_path_text(&bytes, &hashes)?;
                }
                let path = self.legacy.canon_path(&bytes)?;
                allocation(m.heap.path(r, self.legacy.root_accessor, path.as_bytes()))
            }
            Kind::String => strings::concat_parts(m, r, &string_parts, &segments),
        }
    }

    pub fn position_value<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        let Some(position) = self.legacy.position(pos) else {
            return Ok(r.null());
        };
        let Origin::File { path, .. } = &position.info.origin else {
            return Ok(r.null());
        };
        let path = allocation(m.heap.path(r, self.legacy.root_accessor, path))?;
        let path = allocation(m.heap.cell(r, path))?;
        let file = crate::managed_builtins::coerce(self, m, r, path, SrcPos::NONE, false, false)?;
        let mut attrs = vec![
            RootedAttr {
                name: sym::FILE,
                pos: SrcPos::NONE,
                value: allocation(m.heap.cell(r, file))?,
            },
            RootedAttr {
                name: sym::LINE,
                pos: SrcPos::NONE,
                value: allocation(m.heap.cell(r, r.int(i64::from(position.line))))?,
            },
            RootedAttr {
                name: sym::COLUMN,
                pos: SrcPos::NONE,
                value: allocation(m.heap.cell(r, r.int(i64::from(position.column))))?,
            },
        ];
        attrs.sort_unstable_by_key(|a| a.name);
        let set = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, SrcPos::NONE))?;
        Ok(m.heap.bindings_value(r, set))
    }

    fn assert_failure<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        code: &'a LinkedCode<'a>,
        env: EnvRoot<'_, 'h>,
        cond: NodeId,
        pos: SrcPos,
    ) -> crate::Error {
        let shown = crate::show::show_node(&self.legacy, code, cond);
        if let Node::BinOp {
            op: BinOp::Eq,
            lhs,
            rhs,
            ..
        } = *code.node(cond)
        {
            let compared = (|| {
                let a = self.node(m, r, code, lhs, env)?;
                let b = self.node(m, r, code, rhs, env)?;
                let a = allocation(m.heap.cell(r, a))?;
                let b = allocation(m.heap.cell(r, b))?;
                self.assert_equal(m, r, a, b, SrcPos::NONE)
            })();
            if let Err(error) = compared {
                if error.kind == ErrorKind::Assertion {
                    self.legacy.add_trace(
                        &error,
                        pos,
                        msg!(
                            "while evaluating the condition of the assertion '",
                            shown,
                            "'"
                        ),
                    );
                }
                return error;
            }
        }
        self.legacy.error_at(
            ErrorKind::Assertion,
            msg!("assertion '", shown, "' failed"),
            pos,
        )
    }

    #[allow(clippy::collapsible_match)] // Guards would borrow the heap across recursive collection.
    fn assert_equal<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        out: &Roots<'r, 'h>,
        left: CellRoot<'_, 'h>,
        right: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<()> {
        use crate::managed_builtins::{equal, print_for_error, show_type};
        self.legacy.with_stack(||{
            let _guard=self.legacy.enter(pos)?;
            m.with_roots(|m,r|{
                let a=self.force(m,r,left,pos)?;let b=self.force(m,r,right,pos)?;
                if m.heap.same_cell(left,right){return Ok(());}
                let numeric=|v|matches!(v,ValueView::Int(_)|ValueView::Float(_));
                let err=|message:Vec<u8>|self.legacy.error(ErrorKind::Assertion,message);
                if numeric(m.heap.view(a))&&numeric(m.heap.view(b)){
                    if equal(self,m,r,left,right,pos)?{return Ok(());}
                    return Err(err(msg!(show_type(m.heap.view(a))," with value '",print_for_error(self,m,a),"' is not equal to ",show_type(m.heap.view(b))," with value '",print_for_error(self,m,b),"'")));
                }
                if value_type(m.heap.view(a))!=value_type(m.heap.view(b)){
                    return Err(err(msg!(show_type(m.heap.view(a))," of value '",print_for_error(self,m,a),"' is not equal to ",show_type(m.heap.view(b))," of value '",print_for_error(self,m,b),"'")));
                }
                match(m.heap.view(a),m.heap.view(b)){
                    (ValueView::Bool(x),ValueView::Bool(y))if x!=y=>return Err(err(format!("boolean '{x}' is not equal to boolean '{y}'").into_bytes())),
                    (ValueView::String(_),ValueView::String(_))=>if !equal(self,m,r,left,right,pos)?{return Err(err(msg!("string '",print_for_error(self,m,a),"' is not equal to string '",print_for_error(self,m,b),"'")));},
                    (ValueView::Path(x),ValueView::Path(y))=>{
                        let different=x.accessor()!=y.accessor();
                        if different||!equal(self,m,r,left,right,pos)?{return Err(err(msg!("path '",print_for_error(self,m,a),"' is not equal to path '",print_for_error(self,m,b),"'",if different{" because their accessors are different"}else{""})));}
                    }
                    (ValueView::List(x),ValueView::List(y))=>{
                        let len=x.len();
                        if len!=y.len(){return Err(err(msg!("list of size '",len.to_string(),"' is not equal to list of size '",y.len().to_string(),"', left hand side is '",print_for_error(self,m,a),"', right hand side is '",print_for_error(self,m,b),"'")));}
                        for i in 0..len{
                            let(ValueView::List(x),ValueView::List(y))=(m.heap.view(a),m.heap.view(b))else{unreachable!()};
                            let x=x.get(i).unwrap().root(r);let y=y.get(i).unwrap().root(r);
                            self.assert_equal(m,r,x,y,pos).map_err(|error|self.traced(error,pos,&format!("while comparing list element {i}")))?;
                        }
                    }
                    (ValueView::Attrs(x),ValueView::Attrs(y))=>{
                        let x=x.root(r);let y=y.root(r);
                        if self.is_derivation(m,r,x)?&&self.is_derivation(m,r,y)?{
                            let i=m.heap.bindings_view(x).get(sym::OUT_PATH).map(|a|a.value.root(r));
                            let j=m.heap.bindings_view(y).get(sym::OUT_PATH).map(|a|a.value.root(r));
                            if let(Some(i),Some(j))=(i,j){return self.assert_equal(m,r,i,j,pos).map_err(|error|self.traced(error,pos,"while comparing a derivation by its 'outPath' attribute"));}
                        }
                        let xv=m.heap.bindings_view(x);let yv=m.heap.bindings_view(y);
                        if xv.len()!=yv.len(){return Err(err(msg!("attribute names of attribute set '",print_for_error(self,m,a),"' differs from attribute set '",print_for_error(self,m,b),"'")));}
                        let pairs:Vec<_>=xv.iter().zip(yv.iter()).map(|(i,j)|(i.root(r),j.root(r))).collect();
                        for(i,j)in pairs{
                            if i.name!=j.name{
                                if m.heap.bindings_view(y).get(i.name).is_none(){return Err(err(msg!("attribute name '",self.legacy.symbol_ident(i.name),"' is contained in '",print_for_error(self,m,a),"', but not in '",print_for_error(self,m,b),"'")));}
                                if m.heap.bindings_view(x).get(j.name).is_none(){return Err(err(msg!("attribute name '",self.legacy.symbol_ident(j.name),"' is missing in '",print_for_error(self,m,a),"', but is contained in '",print_for_error(self,m,b),"'")));}
                            }
                            if let Err(error)=self.assert_equal(m,r,i.value,j.value,pos){
                                if j.pos.is_some(){self.legacy.add_trace(&error,j.pos,"where right hand side is");}
                                if i.pos.is_some(){self.legacy.add_trace(&error,i.pos,"where left hand side is");}
                                self.legacy.add_trace(&error,pos,msg!("while comparing attribute '",self.legacy.symbol_ident(i.name),"'"));return Err(error);
                            }
                        }
                    }
                    _ if value_type(m.heap.view(a))==crate::value::ValueType::Function=>return Err(err(b"distinct functions and immediate comparisons of identical functions compare as unequal".to_vec())),
                    _=>{}
                }
                let _=out;
                Ok(())
            })
        })
    }

    fn is_derivation<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        set: BindingsRoot<'_, 'h>,
    ) -> Result<bool> {
        let Some(cell) = m
            .heap
            .bindings_view(set)
            .get(sym::TYPE)
            .map(|a| a.value.root(r))
        else {
            return Ok(false);
        };
        let value = self.force(m, r, cell, SrcPos::NONE)?;
        if !matches!(m.heap.view(value), ValueView::String(_)) {
            return Ok(false);
        }
        let value = crate::managed_builtins_strings::materialize(self, m, r, value)?;
        Ok(matches!(m.heap.view(value),ValueView::String(s)if s.bytes()==b"derivation"))
    }

    pub fn call_value<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        out: &Roots<'r, 'h>,
        fun: ValueRoot<'_, 'h>,
        args: &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        #[cfg(brix_parallel)]
        m.safepoint()?;
        self.legacy.with_stack(|| {
            let _depth = self.legacy.enter(pos)?;
            match m.heap.view(fun) {
                ValueView::Thunk { .. }
                | ValueView::App(_)
                | ValueView::Blackhole
                | ValueView::Failed(_) => {
                    let cell = allocation(m.heap.cell(out, fun))?;
                    let fun = self.force(m, out, cell, pos)?;
                    self.apply(m, out, fun, args, pos)
                }
                _ => self.apply(m, out, fun, args, pos),
            }
        })
    }

    /// Call once in a temporary frame owned by the caller. The first result
    /// already belongs to that frame, so it needs no child-frame promotion.
    /// Callers must discard the frame after consuming the result; repeated
    /// calls in a long-lived frame must continue to use `call_value` instead.
    pub(crate) fn call_value_in_frame<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: ValueRoot<'_, 'h>,
        args: &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        #[cfg(brix_parallel)]
        m.safepoint()?;
        self.legacy.with_stack(|| {
            let _depth = self.legacy.enter(pos)?;
            match m.heap.view(fun) {
                ValueView::Thunk { .. }
                | ValueView::App(_)
                | ValueView::Blackhole
                | ValueView::Failed(_) => {
                    let cell = allocation(m.heap.cell(roots, fun))?;
                    let fun = self.force(m, roots, cell, pos)?;
                    self.apply_in_frame(m, roots, fun, args, pos)
                }
                _ => self.apply_in_frame(m, roots, fun, args, pos),
            }
        })
    }

    /// One reduction may use this caller-owned temporary frame. Any further
    /// reductions keep `apply`'s bounded frames and reusable result slot.
    fn apply_in_frame<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: ValueRoot<'_, 'h>,
        mut args: &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        if args.is_empty() {
            return Ok(roots.copy(fun));
        }
        let result = self.apply_step(m, roots, fun, &mut args, pos)?;
        if args.is_empty() {
            Ok(result)
        } else {
            self.apply(m, roots, result, args, pos)
        }
    }

    fn apply<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        out: &Roots<'r, 'h>,
        fun: ValueRoot<'_, 'h>,
        mut args: &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        if args.is_empty() {
            return Ok(out.copy(fun));
        }
        // The caller's capability keeps the registry alive. Each reduction has
        // its own bounded frame without cloning an owning registry handle.
        // Promote the first result once, whether it finishes the call or needs
        // more arguments. No initial carry copy of fun is needed.
        let result = out.with_roots(|r| -> Result<_> {
            let value = self.apply_step(m, r, fun, &mut args, pos)?;
            Ok(out.slot(value))
        })?;
        // Only the current result crosses subsequent iterations. Reuse its slot
        // so a long curried call cannot accumulate roots in the caller's frame.
        while !args.is_empty() {
            out.with_roots(|r| -> Result<_> {
                let value = self.apply_step(m, r, result.root(), &mut args, pos)?;
                result.set(value);
                Ok(())
            })?;
        }
        Ok(result.root())
    }

    fn apply_step<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        cur: ValueRoot<'_, 'h>,
        remaining_args: &mut &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        let args = *remaining_args;
        let (result, consumed) = match m.heap.view(cur) {
            ValueView::Lambda { node, .. } => (self.call_lambda(m, r, cur, node, args[0], pos)?, 1),
            ValueView::PrimOp(id) => {
                let arity = self.primop(id).arity as usize;
                if args.len() < arity {
                    (self.partial(m, r, cur, args)?, args.len())
                } else {
                    (self.call_primop(m, r, id, &args[..arity], pos)?, arity)
                }
            }
            ValueView::PrimOpApp(_) => {
                let mut done: smallvec::SmallVec<[_; 4]> = smallvec::SmallVec::new();
                let mut head = r.copy(cur);
                let id = loop {
                    match m.heap.view(head) {
                        ValueView::PrimOpApp(app) => {
                            let fun = app.fun().root(r);
                            done.push(app.arg().root(r));
                            head = m.heap.value(r, fun);
                        }
                        ValueView::PrimOp(id) => break id,
                        _ => unreachable!("partial application is headed by a primop"),
                    }
                };
                done.reverse();
                let remaining = self.primop(id).arity as usize - done.len();
                if args.len() < remaining {
                    (self.partial(m, r, cur, args)?, args.len())
                } else {
                    done.extend(args[..remaining].iter().map(|&arg| r.copy(arg)));
                    (self.call_primop(m, r, id, &done, pos)?, remaining)
                }
            }
            ValueView::Attrs(set) if set.get(sym::FUNCTOR).is_some() => {
                let functor = set.get(sym::FUNCTOR).unwrap().root(r);
                let self_cell = allocation(m.heap.cell(r, cur))?;
                let fun = self.force(m, r, functor.value, functor.pos)?;
                (self.call_value(m, r, fun, &[self_cell, r.copy(args[0])], functor.pos).map_err(|e| self.traced(e, pos, "while calling a functor (an attribute set with a '__functor' attribute)"))?, 1)
            }
            _ => return Err(self.not_a_function(m, cur, pos)),
        };
        // Commit consumption only after the reduction succeeds. Keeping this
        // state in the caller avoids returning a rooted-value/count aggregate
        // through an out pointer, without changing frames or call-depth guards.
        *remaining_args = &args[consumed..];
        Ok(result)
    }

    #[cold]
    #[inline(never)]
    fn not_a_function<'h>(
        &self,
        m: &Machine<'h>,
        value: ValueRoot<'_, 'h>,
        pos: SrcPos,
    ) -> crate::Error {
        self.legacy.error_at(
            ErrorKind::TypeError,
            msg!(
                "attempt to call something which is not a function but ",
                crate::managed_builtins::show_type(m.heap.view(value)),
                ": ",
                crate::managed_builtins::print_for_error(self, m, value)
            ),
            pos,
        )
    }

    fn partial<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        fun: ValueRoot<'_, 'h>,
        args: &[CellRoot<'_, 'h>],
    ) -> Result<ValueRoot<'r, 'h>> {
        let mut fun = r.copy(fun);
        for &arg in args {
            let cell = allocation(m.heap.cell(r, fun))?;
            fun = allocation(m.heap.app(r, cell, arg, true))?;
        }
        Ok(fun)
    }

    fn call_primop<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        id: usize,
        args: &[CellRoot<'_, 'h>],
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        let primop = self.primop(id);
        #[cfg(brix_parallel)]
        if m.background.is_some()
            && (primop.has_callback || ordered_builtin(primop.dispatch_name))
        {
            crate::runtime::note_deferral(format_args!("builtins.{}", primop.dispatch_name));
            return Err(crate::error::deferred());
        }
        let callback = if primop.has_callback {
            self.callbacks.borrow().get(primop.name).cloned()
        } else {
            None
        };
        let result = match callback {
            Some(callback) => {
                crate::managed_api::invoke_callback(self, m, r, callback.as_ref(), args)
            }
            None => (primop.handler)(self, m, r, primop.dispatch_name, args, SrcPos::NONE),
        };
        result.inspect_err(|error| {
            if primop.add_trace {
                self.legacy.add_trace(
                    error,
                    pos,
                    format!("while calling the '{}' builtin", primop.name),
                );
            }
        })
    }

    fn call_lambda<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        function: ValueRoot<'_, 'h>,
        node: GlobalNode,
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        let dispatch = self.lambda_dispatch(node);
        let code = dispatch.code;
        let lambda = dispatch.definition;
        let frame = if lambda.formals.is_none() {
            allocation(m.heap.env_one_from_lambda(r, function, arg))?
        } else {
            self.match_lambda_formals(m, r, function, (code, lambda), arg, pos)?
        };
        let body = match dispatch.body.get() {
            Some(body) => body,
            None => {
                let body = self.compiled(code, lambda.body, 0);
                dispatch.body.set(Some(body));
                body
            }
        };
        body(self, m, r, frame).inspect_err(|error| {
            self.trace_lambda_error(error, lambda, self.legacy.src_pos(code, lambda.pos), pos);
        })
    }

    // Keep the larger formal-matching state out of ordinary lambda and primop
    // reductions. This still executes in the same bounded application frame.
    #[inline(never)]
    fn match_lambda_formals<'r, 'h>(
        &self,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        function: ValueRoot<'_, 'h>,
        definition: (&'a LinkedCode<'a>, &'a crate::code::Lambda<'a>),
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<EnvRoot<'r, 'h>> {
        let (code, lambda) = definition;
        let ValueView::Lambda { env, .. } = m.heap.view(function) else {
            unreachable!("validated lambda")
        };
        let env = env.root(r);
        let lambda_pos = self.legacy.src_pos(code, lambda.pos);
        let frame = self.frame(m, r, Some(env), lambda.env_size())?;
        let name = lambda.name.map_or(&b"anonymous lambda"[..], |name| {
            self.legacy.symbol_name(name)
        });
        let formals = lambda.formals.as_ref().expect("formal lambda");
        let attrs = crate::managed_builtins::force_attrs(
            self,
            m,
            r,
            arg,
            lambda_pos,
            "while evaluating the value passed for the lambda argument",
        )
        .map_err(|e| {
            if pos.is_some() {
                self.traced(e, pos, "from call site")
            } else {
                e
            }
        })?;
        let offset = usize::from(lambda.arg.is_some());
        if offset != 0 {
            m.heap.env_set(frame, 0, arg);
        }
        let mut used = 0;
        for (i, formal) in formals.in_id_order() {
            let found = m
                .heap
                .bindings_view(attrs)
                .get(formal.name)
                .map(|a| a.value.root(r));
            let value = match found {
                Some(value) => {
                    used += 1;
                    value
                }
                None => match formal.default {
                    Some(node) => self.lazy_cell(m, r, code, frame, self.lazy(code, node))?,
                    None => {
                        let error = self.legacy.error_at(
                            ErrorKind::TypeError,
                            msg!(
                                "function '",
                                name,
                                "' called without required argument '",
                                self.legacy.symbol_ident(formal.name),
                                "'"
                            ),
                            lambda_pos,
                        );
                        return Err(self.traced(error, pos, "from call site"));
                    }
                },
            };
            m.heap.env_set(frame, offset + i, value);
        }
        if !formals.ellipsis && used != m.heap.bindings_view(attrs).len() {
            if let Some(unexpected) = m
                .heap
                .bindings_view(attrs)
                .iter()
                .find(|a| !formals.has(a.name))
            {
                let error = self.legacy.error_at(
                    ErrorKind::TypeError,
                    msg!(
                        "function '",
                        name,
                        "' called with unexpected argument '",
                        self.legacy.symbol_ident(unexpected.name),
                        "'"
                    ),
                    lambda_pos,
                );
                error.set_suggestions(crate::error::best_suggestions_bytes(
                    formals
                        .formals
                        .iter()
                        .map(|f| self.legacy.symbol_name(f.name)),
                    self.legacy.symbol_name(unexpected.name),
                ));
                return Err(self.traced(error, pos, "from call site"));
            }
        }
        Ok(frame)
    }

    #[cold]
    #[inline(never)]
    fn trace_lambda_error(
        &self,
        error: &crate::Error,
        lambda: &crate::code::Lambda<'a>,
        lambda_pos: SrcPos,
        pos: SrcPos,
    ) {
        if self.legacy.config.show_trace {
            let label = lambda.name.map_or_else(
                || b"anonymous lambda".to_vec(),
                |name| msg!("'", self.legacy.symbol_name(name), "'"),
            );
            self.legacy
                .add_trace(error, lambda_pos, msg!("while calling ", label));
            if pos.is_some() {
                self.legacy.add_trace(error, pos, "from call site");
            }
        }
    }
}

fn value_type(value: ValueView<'_, '_>) -> crate::value::ValueType {
    use crate::value::ValueType as T;
    match value {
        ValueView::Int(_) => T::Int,
        ValueView::Float(_) => T::Float,
        ValueView::Bool(_) => T::Bool,
        ValueView::Null => T::Null,
        ValueView::String(_) => T::String,
        ValueView::Path(_) => T::Path,
        ValueView::Attrs(_) => T::Attrs,
        ValueView::List(_) => T::List,
        ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => T::Function,
        _ => T::Thunk,
    }
}

impl Code for Language<'_> {
    fn with_stack<R>(&self, callback: impl FnOnce() -> R) -> R {
        self.legacy.with_stack(callback)
    }
    fn eval<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        node: GlobalNode,
    ) -> Result<ValueRoot<'r, 'h>> {
        // Machine::force owns this thunk's frame and snapshots its suspension.
        self.thunk_dispatch(node)(self, machine, roots, env)
    }
    fn call<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.legacy.with_stack(|| {
            let _depth = self.legacy.enter(pos)?;
            let value = self.force(machine, roots, fun, pos)?;
            self.apply(machine, roots, value, &[arg], pos)
        })
    }
    fn call_in_frame<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        // Machine owns this suspended application's bounded frame. Keep the
        // same stack/depth guards as call, but avoid a redundant first-step
        // frame and result promotion before Machine immediately stores it.
        self.legacy.with_stack(|| {
            let _depth = self.legacy.enter(pos)?;
            let function = self.force(machine, roots, fun, pos)?;
            self.apply_in_frame(machine, roots, function, &[arg], pos)
        })
    }
    fn position(&self, pos: SrcPos) -> Option<crate::error::Position> {
        self.legacy.position(pos)
    }
}

/// Bootstrap only: copy the evaluator's permanent initial graph without
/// forcing anything. Memoized cells/frames preserve cycles and sharing.
struct Importer<'l, 'a, 'm, 'r, 'h> {
    language: &'l Language<'a>,
    machine: &'m mut Machine<'h>,
    roots: &'m Roots<'r, 'h>,
    cells: HashMap<usize, CellRoot<'r, 'h>>,
    envs: HashMap<usize, EnvRoot<'r, 'h>>,
}
impl<'a, 'r, 'h> Importer<'_, 'a, '_, 'r, 'h> {
    fn cell(&mut self, old: ValueRef<'a>) -> Result<CellRoot<'r, 'h>> {
        let key = old as *const _ as usize;
        if let Some(cell) = self.cells.get(&key) {
            return Ok(*cell);
        }
        let cell = allocation(self.machine.heap.cell(self.roots, self.roots.blackhole()))?;
        self.cells.insert(key, cell);
        let value = self.value(old.get())?;
        self.machine.heap.store(cell, value);
        Ok(cell)
    }
    fn env(&mut self, old: Env<'a>) -> Result<EnvRoot<'r, 'h>> {
        let key = old.slots().as_ptr() as usize;
        if let Some(env) = self.envs.get(&key) {
            return Ok(*env);
        }
        // Lexical parent links are acyclic. Cells may refer back to this frame,
        // so register the frame before copying any slots.
        let placeholder = allocation(self.machine.heap.cell(self.roots, self.roots.blackhole()))?;
        let env = allocation(self.machine.heap.env(
            self.roots,
            None,
            &vec![placeholder; old.len()],
        ))?;
        self.envs.insert(key, env);
        let up = old.up().map(|up| self.env(up)).transpose()?;
        self.machine.heap.env_set_up(env, up);
        for (i, slot) in old.slots().iter().enumerate() {
            let cell = self.cell(slot.get())?;
            self.machine.heap.env_set(env, i, cell);
        }
        Ok(env)
    }
    fn value(&mut self, value: Value<'a>) -> Result<ValueRoot<'r, 'h>> {
        let roots = self.roots;
        Ok(match value {
            Value::Int(i) => roots.int(i),
            Value::Float(f) => roots.float(f),
            Value::Bool(b) => roots.bool(b),
            Value::Null => roots.null(),
            Value::Blackhole => roots.blackhole(),
            Value::String(s) => {
                let mut contexts = Vec::new();
                for c in s.raw_context() {
                    let input = match c {
                        ContextElem::LazyHash { offset, tree } => ContextInput::LazyHash {
                            offset: *offset,
                            tree: *tree,
                        },
                        ContextElem::LazyPath(tree) => ContextInput::LazyPath(*tree),
                        ContextElem::Opaque(p) => ContextInput::Opaque(allocation(
                            self.machine.heap.bytes(roots, p.as_bytes()),
                        )?),
                        ContextElem::DrvDeep(p) => ContextInput::DrvDeep(allocation(
                            self.machine.heap.bytes(roots, p.as_bytes()),
                        )?),
                        ContextElem::Path(p) => ContextInput::Path(allocation(
                            self.machine.heap.bytes(roots, p.as_bytes()),
                        )?),
                        ContextElem::Built { drv, output } => ContextInput::Built {
                            drv: allocation(self.machine.heap.bytes(roots, drv.as_bytes()))?,
                            output: allocation(self.machine.heap.bytes(roots, output))?,
                        },
                    };
                    contexts.push(input);
                }
                let context = allocation(self.machine.heap.contexts(roots, &contexts))?;
                allocation(
                    self.machine
                        .heap
                        .string_with_context(roots, s.raw_bytes(), context),
                )?
            }
            Value::Path(path) => {
                allocation(self.machine.heap.path(roots, path.accessor, path.path))?
            }
            Value::Attrs(set) => {
                let mut attrs = Vec::with_capacity(set.len());
                for a in set.iter() {
                    attrs.push(RootedAttr {
                        name: a.name,
                        pos: a.pos,
                        value: self.cell(a.value)?,
                    });
                }
                let set = allocation(bindings::from_sorted(
                    &mut self.machine.heap,
                    roots,
                    &attrs,
                    set.pos(),
                ))?;
                self.machine.heap.bindings_value(roots, set)
            }
            Value::List(list) => {
                let values = list
                    .elems
                    .iter()
                    .map(|&cell| self.cell(cell))
                    .collect::<Result<Vec<_>>>()?;
                allocation(self.machine.heap.list(roots, &values))?
            }
            Value::Lambda { env, node } => {
                let env = self.env(env)?;
                self.machine.heap.lambda(roots, env, node)
            }
            Value::Thunk { env, node } => {
                let env = self.env(env)?;
                self.machine.heap.thunk(roots, env, node)
            }
            Value::PrimOp(p) => self.machine.heap.primop(roots, self.language.primop_id(p)),
            Value::App(app) => {
                let fun = self.cell(app.fun)?;
                let arg = self.cell(app.arg)?;
                allocation(self.machine.heap.app(roots, fun, arg, false))?
            }
            Value::PrimOpApp(app) => {
                let fun = self.cell(app.fun)?;
                let arg = self.cell(app.arg)?;
                allocation(self.machine.heap.app(roots, fun, arg, true))?
            }
            Value::Failed(error) => allocation(self.machine.heap.failed(roots, error.clone()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;

    fn load_test_code<'a>(language: &Language<'a>, source: &[u8]) -> &'a LinkedCode<'a> {
        let expr = language
            .legacy()
            .parse_source(source, b"/", &Origin::String)
            .unwrap();
        language
            .legacy()
            .load(&expr, Origin::String, source.to_vec(), None, false)
            .unwrap()
    }

    #[test]
    fn direct_thunk_dispatch_aliases_lazy_slots_across_pages_and_heaps() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        let page_size = 1usize << crate::code::NODE_PAGE_BITS;
        let source = format!(
            "[{}]",
            (0..page_size + 3)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let code = load_test_code(&language, source.as_bytes());
        assert!(language.thunk_dispatch.get().is_empty());
        assert!(language.closures.get().is_empty());
        let literals: Vec<_> = code
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                if let Node::Int(value) = node {
                    Some((NodeId(index as u32), *value))
                } else {
                    None
                }
            })
            .collect();
        let first = literals[0];
        let last = *literals
            .iter()
            .find(|(node, _)| node.0 as usize >= page_size)
            .unwrap();
        let first_global = GlobalNode(code.node_base + first.0 .0);
        let first_eval = language.thunk_dispatch(first_global);
        let file_page = (code.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        let slots = language.closures.get()[file_page].get();
        assert_eq!(slots.iter().filter(|slot| slot.get().is_some()).count(), 1);
        let first_page = language.thunk_dispatch.get()[file_page].get();
        assert!(std::ptr::eq(
            &first_page[first.0 .0 as usize],
            &slots[first.0 .0 as usize]
        ));
        // Compiling through the ordinary strict path updates these exact slots,
        // including any stack-growth wrapper chosen by the original depth.
        let another = literals[1].0;
        let strict_eval = language.compiled(code, another, 8);
        assert!(std::ptr::eq(
            strict_eval,
            language.thunk_dispatch(GlobalNode(code.node_base + another.0))
        ));
        let last_global = GlobalNode(code.node_base + last.0 .0);
        language.thunk_dispatch(last_global);
        let last_page = language.thunk_dispatch.get()[file_page + 1].get();
        assert_eq!(last_page.len(), code.nodes.len() - page_size);
        assert!(std::ptr::eq(
            &last_page[last.0 .0 as usize - page_size],
            &slots[last.0 .0 as usize]
        ));

        let old_capacity = language.thunk_dispatch.get().len();
        loop {
            let later = load_test_code(&language, b"42");
            if (later.node_base >> crate::code::NODE_PAGE_BITS) as usize >= old_capacity {
                language.thunk_dispatch(GlobalNode(later.node_base + later.root.0));
                break;
            }
        }
        assert!(std::ptr::eq(
            first_page,
            language.thunk_dispatch.get()[file_page].get()
        ));
        assert!(std::ptr::eq(
            first_eval,
            language.thunk_dispatch(first_global)
        ));
        for _ in 0..2 {
            with_owned_heap(brix_gc::Config { stress: true, ..Default::default() }, |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|m, roots| {
                    let env = language.base_env(m, roots).unwrap();
                    for (global, expected) in [(first_global, first.1), (last_global, last.1)] {
                        let thunk = m.heap.thunk_cell(roots, env, global).unwrap();
                        let value = language.force(m, roots, thunk, SrcPos::NONE).unwrap();
                        m.heap.collect().unwrap();
                        assert!(matches!(m.heap.view(value), ValueView::Int(value) if value == expected));
                    }
                });
            }).unwrap();
        }
    }

    #[test]
    fn prepared_lambda_dispatch_is_lazy_stable_and_shared_across_heaps() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        let code = load_test_code(&language, b"{ x }: x");
        let node = GlobalNode(code.node_base + code.root.0);
        assert!(language.lambda_dispatch.get().is_empty());
        let dispatch = language.lambda_dispatch(node);
        assert!(dispatch.body.get().is_none());
        assert!(language.closures.get().is_empty());
        assert_eq!(
            language
                .lambda_dispatch
                .get()
                .iter()
                .map(|page| page.get().len())
                .sum::<usize>(),
            code.nodes.len(),
            "the final dispatch page must not allocate its unused tail"
        );

        let old_capacity = language.lambda_dispatch.get().len();
        loop {
            let later = load_test_code(&language, b"y: y");
            if (later.node_base >> crate::code::NODE_PAGE_BITS) as usize >= old_capacity {
                language.lambda_dispatch(GlobalNode(later.node_base + later.root.0));
                break;
            }
        }
        assert!(std::ptr::eq(dispatch, language.lambda_dispatch(node)));

        for invocation in 0..2 {
            let previous_body = dispatch.body.get();
            with_owned_heap(
                brix_gc::Config {
                    stress: true,
                    ..Default::default()
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|m, roots| {
                        let env = language.base_env(m, roots).unwrap();
                        let function = m.heap.lambda(roots, env, node);
                        let bad = m.heap.cell(roots, roots.int(0)).unwrap();
                        assert!(language
                            .call_lambda(m, roots, function, node, bad, SrcPos::NONE)
                            .is_err());
                        if invocation == 0 {
                            assert!(
                                dispatch.body.get().is_none(),
                                "reject formals before compiling the body"
                            );
                        }
                        let value = m.heap.cell(roots, roots.int(42)).unwrap();
                        let set = m
                            .heap
                            .alloc_flat_bindings(
                                roots,
                                SrcPos::NONE,
                                &[RootedAttr {
                                    name: dispatch.definition.formals.as_ref().unwrap().formals[0]
                                        .name,
                                    pos: SrcPos::NONE,
                                    value,
                                }],
                            )
                            .unwrap();
                        let attrs = m.heap.bindings_value(roots, set);
                        let arg = m.heap.cell(roots, attrs).unwrap();
                        let result = language
                            .call_lambda(m, roots, function, node, arg, SrcPos::NONE)
                            .unwrap();
                        m.heap.collect().unwrap();
                        assert!(matches!(m.heap.view(result), ValueView::Int(42)));
                    });
                },
            )
            .unwrap();
            let body = dispatch
                .body
                .get()
                .expect("successful calls cache the body evaluator");
            if let Some(previous_body) = previous_body {
                assert!(std::ptr::eq(previous_body, body));
            }
        }
    }

    #[test]
    fn managed_closure_slots_are_lazy_per_file_and_stable_during_recursive_compilation() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        for _ in 0..32 {
            load_test_code(&language, b"0");
        }
        let code = load_test_code(&language, b"1 + (2 + (3 + 4))");
        assert!(
            language.closures.get().is_empty(),
            "linking must allocate no managed closure slots"
        );
        let compiled = language.compiled(code, code.root, 0);
        let page = (code.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        let slots = language.closures.get()[page].get();
        assert_eq!(slots.len(), code.nodes.len());
        assert!(slots.iter().filter(|slot| slot.get().is_some()).count() > 1);
        assert_eq!(
            language
                .closures
                .get()
                .iter()
                .map(|file| file.get().len())
                .sum::<usize>(),
            code.nodes.len()
        );
        assert!(std::ptr::eq(
            compiled,
            language.compiled(code, code.root, 0)
        ));
        let old_capacity = language.closures.get().len();
        loop {
            let later = load_test_code(&language, b"42");
            if (later.node_base >> crate::code::NODE_PAGE_BITS) as usize >= old_capacity {
                language.compiled(later, later.root, 0);
                break;
            }
        }
        assert!(language.closures.get().len() > old_capacity);
        assert!(std::ptr::eq(slots, language.closures.get()[page].get()));
        assert!(std::ptr::eq(
            compiled,
            language.compiled(code, code.root, 0)
        ));
    }

    #[test]
    fn managed_closure_tables_keep_shared_code_literal_ids_language_local() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let first = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        let second = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        let shared = load_test_code(&first, b"\"shared\"");
        let other = load_test_code(&first, b"\"other\"");
        second.literal_id(other, other.root);
        let first_eval = first.compiled(shared, shared.root, 0);
        let second_eval = second.compiled(shared, shared.root, 0);
        assert_eq!(first.literal_id(shared, shared.root).0, 0);
        assert_eq!(second.literal_id(shared, shared.root).0, 1);
        let page = (shared.node_base >> crate::code::NODE_PAGE_BITS) as usize;
        assert!(!std::ptr::eq(
            first.closures.get()[page].get(),
            second.closures.get()[page].get()
        ));
        assert!(!std::ptr::eq(first_eval, second_eval));
        for (language, eval, id) in [(&first, first_eval, 0), (&second, second_eval, 1)] {
            with_owned_heap(brix_gc::Config { stress: true, ..Default::default() }, |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let env = machine.heap.env(roots, None, &[]).unwrap();
                    let value = eval(language, machine, roots, env).unwrap();
                    machine.heap.collect().unwrap();
                    assert!(matches!(machine.heap.view(value), ValueView::String(s) if s.bytes() == b"shared"));
                    assert!(machine.literals[id].is_some());
                    if id == 1 { assert!(machine.literals[0].is_none()); }
                });
            }).unwrap();
        }
    }

    fn cases(cases: &[(&str, &str)], stress: bool) {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = Evaluator::new(&arena, crate::Config::default(), &host);
        let language = Language::new(legacy);
        with_owned_heap(
            brix_gc::Config {
                workers: 0,
                stress,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 1024,
                chunk_bytes: 256,
            },
            |heap| {
                let mut machine = Machine::new(heap);
                for &(source, expected) in cases {
                    machine.with_roots(|machine, roots| {
                        let value = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .unwrap_or_else(|e| panic!("{source}: {}", e.render(true)));
                        let actual =
                            crate::managed_builtins::print_for_error(&language, machine, value);
                        assert_eq!(actual, expected.as_bytes(), "{source}");
                        machine.heap.collect().unwrap();
                        assert_eq!(
                            crate::managed_builtins::print_for_error(&language, machine, value),
                            expected.as_bytes(),
                            "after collection: {source}"
                        );
                    });
                }
                assert!(machine.heap.stats().collections > 0);
            },
        )
        .unwrap();
    }

    #[test]
    fn dense_literal_ids_preserve_cache_identity_across_machines_and_moves() {
        assert_eq!(size_of::<LiteralId>(), size_of::<GlobalNode>());
        assert_eq!(
            size_of::<Lazy>(),
            16,
            "literal IDs must not enlarge lazy plans"
        );
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        let load = |source: &[u8]| {
            let expr = language
                .legacy()
                .parse_source(source, b"/", &Origin::String)
                .unwrap();
            language
                .legacy()
                .load(&expr, Origin::String, source.to_vec(), None, false)
                .unwrap()
        };
        let first = load(b"\"shared literal\"");
        let second = load(b"\"shared literal\"");
        // Literal syntax remains ASCII; its source directory may contain
        // arbitrary filesystem bytes, which the imported path must preserve.
        let path_source = b"./file";
        let path_expr = language
            .legacy()
            .parse_source(path_source, b"/literal/caf\xff", &Origin::String)
            .unwrap();
        let path = language
            .legacy()
            .load(
                &path_expr,
                Origin::String,
                path_source.to_vec(),
                None,
                false,
            )
            .unwrap();
        let first_id = language.literal_id(first, first.root);
        let second_id = language.literal_id(second, second.root);
        let path_id = language.literal_id(path, path.root);
        assert_eq!(first_id, LiteralId(0));
        assert_eq!(second_id, LiteralId(1));
        assert_eq!(path_id, LiteralId(2));
        let lazy = language.lazy(first, first.root);
        let strict = language.compiled(first, first.root, 0);
        assert_eq!(language.literal_ids.borrow().len(), 3);
        assert!(matches!(lazy, Lazy::Literal { id, .. } if id == first_id));
        // Compiled closures and lazy plans belong to Language, but leases must
        // belong independently to each Machine and its generative heap brand.
        for _ in 0..2 {
            with_owned_heap(brix_gc::Config { stress: true, chunk_bytes: 256, ..Default::default() }, |heap| {
                let mut machine = Machine::new(heap);
                assert!(machine.literals.is_empty());
                machine.with_roots(|m, roots| {
                    let env = m.heap.env(roots, None, &[]).unwrap();
                    let original = language.lazy_cell(m, roots, first, env, lazy).unwrap();
                    let before = m.heap.used_bytes();
                    let value = strict(&language, m, roots, env).unwrap();
                    assert!(matches!(m.heap.view(value), ValueView::String(s) if s.bytes() == b"shared literal"));
                    assert_eq!(m.heap.used_bytes(), before);
                    let cached = m.literals[first_id.0 as usize].as_ref().unwrap().root();
                    assert!(m.heap.same_cell(original, cached));
                    let separate = language.literal_cell(m, roots, second, second.root, second_id).unwrap();
                    assert!(!m.heap.same_cell(original, separate));
                    language.literal_cell(m, roots, path, path.root, path_id).unwrap();
                });
                for _ in 0..3 {
                    machine.heap.collect().unwrap();
                    machine.with_roots(|m, roots| {
                        let before = m.heap.used_bytes();
                        let cell = language.literal_cell(m, roots, first, first.root, first_id).unwrap();
                        assert!(matches!(m.heap.read(cell), ValueView::String(s) if s.bytes() == b"shared literal"));
                        let cell = language.literal_cell(m, roots, path, path.root, path_id).unwrap();
                        let ValueView::Path(value) = m.heap.read(cell) else { panic!("path literal") };
                        assert_eq!(value.bytes(), b"/literal/caf\xff/file");
                        assert_eq!(value.literal(), Some(GlobalNode(path.node_base + path.root.0)));
                        assert_eq!(m.heap.used_bytes(), before);
                    });
                }
                assert_eq!(machine.literals.len(), 3);
                assert!(machine.literals.iter().all(Option::is_some));
            }).unwrap();
        }
        assert_eq!(language.literal_ids.borrow().len(), 3);
    }

    #[test]
    fn managed_expressions_survive_collection_at_every_allocation() {
        cases(
            &[
                ("1 + 2 * 3", "7"),
                ("let x = 20; y = x + 2; in y + x", "42"),
                ("(rec { x = 21; y = x + x; }).y", "42"),
                (
                    "(rec { x = 1; y = x + x; __overrides = { x = 21; }; }).y",
                    "42",
                ),
                (
                    "let a = { x = 20; }; in (rec { inherit (a) x; y = x + 22; }).y",
                    "42",
                ),
                ("with { x = 20; }; with { y = 22; }; x + y", "42"),
                ("let name = \"x\"; in { ${name} = 42; }.${name}", "42"),
                ("(rec { x = x; }) ? x", "true"),
                ("({ x = 1; }.x.y or 42)", "42"),
                ("let a = { x = 20 + 22; }; in builtins.seq a.x a.x", "42"),
                ("{ x = throw \"keep lazy\"; }.missing or 42", "42"),
                ("({ x = { y = 42; }; }.x).y", "42"),
                (
                    r#"let s = builtins.appendContext "abc" { "/nix/store/00000000000000000000000000000000-test".path = true; }; in builtins.getContext "${s}" == builtins.getContext s"#,
                    "true",
                ),
                ("if false && (throw \"unforced\") then 0 else 42", "42"),
                (
                    "assert true || (throw \"unforced\"); if false -> false then 42 else 0",
                    "42",
                ),
                ("(x: y: x + y) 20 22", "42"),
                ("({ x, y ? x + 2 }: x + y) { x = 20; }", "42"),
                ("({ x, ... }@all: x + all.y) { x = 20; y = 22; }", "42"),
                ("({ __functor = self: x: self.n + x; n = 20; }) 22", "42"),
                ("let add20 = builtins.add 20; in add20 22", "42"),
                ("builtins.elemAt ([ 0 42 ] ++ [ 1 ]) 1", "42"),
                ("({ x = 1; } // { x = 42; }).x", "42"),
                ("[ 1 { x = 2; } ] == [ 1 { x = 2; } ]", "true"),
                ("[ 1 ] != [ 2 ]", "true"),
                ("\"a${\"b\"}c\"", "\"abc\""),
                ("1 + 2.5", "3.5"),
                ("/a + \"/b/../c\"", "/a/c"),
                ("__curPos", "null"),
                (
                    "let sum = n: if n == 0 then 0 else n + sum (n - 1); in sum 20",
                    "210",
                ),
            ],
            true,
        );
    }

    #[test]
    fn application_steps_consume_arguments_only_after_success() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        with_owned_heap(
            brix_gc::Config {
                workers: 0,
                stress: true,
                chunk_bytes: 256,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 512,
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let function = language
                        .eval_str(machine, roots, b"x: y: x + y", "/")
                        .unwrap();
                    let a = machine.heap.cell(roots, roots.int(20)).unwrap();
                    let b = machine.heap.cell(roots, roots.int(22)).unwrap();
                    let extra = machine.heap.cell(roots, roots.int(99)).unwrap();
                    let args = [a, b, extra];
                    let mut remaining = &args[..];
                    let next = language
                        .apply_step(machine, roots, function, &mut remaining, SrcPos::NONE)
                        .unwrap();
                    assert_eq!(remaining.len(), 2);
                    machine.heap.collect().unwrap();
                    let result = language
                        .apply_step(machine, roots, next, &mut remaining, SrcPos::NONE)
                        .unwrap();
                    assert!(matches!(machine.heap.view(result), ValueView::Int(42)));
                    assert_eq!(remaining.len(), 1);
                    let before = remaining.as_ptr();
                    assert!(language
                        .apply_step(machine, roots, result, &mut remaining, SrcPos::NONE)
                        .is_err());
                    assert_eq!(remaining.as_ptr(), before);
                    assert_eq!(remaining.len(), 1);
                    assert!(machine.heap.same_cell(remaining[0], extra));

                    let function = language
                        .eval_str(machine, roots, b"builtins.add", "/")
                        .unwrap();
                    let mut first = &args[..1];
                    let partial = language
                        .apply_step(machine, roots, function, &mut first, SrcPos::NONE)
                        .unwrap();
                    assert!(first.is_empty());
                    machine.heap.collect().unwrap();
                    let mut second = &args[1..2];
                    let result = language
                        .apply_step(machine, roots, partial, &mut second, SrcPos::NONE)
                        .unwrap();
                    assert!(second.is_empty());
                    assert!(matches!(machine.heap.view(result), ValueView::Int(42)));

                    let function = language
                        .eval_str(machine, roots, b"x: throw \"step failed\"", "/")
                        .unwrap();
                    let mut failed = &args[..];
                    assert!(language
                        .apply_step(machine, roots, function, &mut failed, SrcPos::NONE)
                        .is_err());
                    assert_eq!(failed.as_ptr(), args.as_ptr());
                    assert_eq!(failed.len(), args.len());
                    machine.heap.collect().unwrap();
                    assert!(matches!(machine.heap.read(a), ValueView::Int(20)));
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn compiled_environment_variables_preserve_levels_and_lazy_slots() {
        let expressions = [
            ("(x: x + 1) 41", "42"),
            ("(x: y: x + y) 40 2", "42"),
            ("(a: b: c: d: a + b + c + d) 10 11 12 9", "42"),
            ("let a = 21; in (x: y: z: a + z) 1 2 21", "42"),
            ("(rec { a = b; b = 42; }).a", "42"),
            ("let x = y; y = 42; in (z: x + z) 0", "42"),
            ("({ x ? y, y ? 42 }: x) {}", "42"),
            ("with { a = 41; }; a + 1", "42"),
        ];
        cases(&expressions, false);
        cases(&expressions, true);
    }

    #[test]
    fn single_step_and_curried_applications_preserve_results_under_gc() {
        let expressions = [
            ("(x: x + 1) 41", "42"),
            ("builtins.add 20 22", "42"),
            ("(x: y: z: x + y + z) 10 20 12", "42"),
            ("builtins.seq 0 (x: y: x + y) 20 22", "42"),
            ("let f = builtins.add 20; in f 22", "42"),
            (
                "let f = builtins.substring 1; g = f 2; in g \"a42b\"",
                "\"42\"",
            ),
            (
                "let f = builtins.foldl' (acc: x: acc + x); in f 0 [ 20 22 ]",
                "42",
            ),
            (
                "({ __functor = self: x: y: self.n + x + y; n = 10; }) 20 12",
                "42",
            ),
            ("(x: y: x) 42 (throw \"unused\")", "42"),
        ];
        cases(&expressions, false);
        cases(&expressions, true);
    }

    #[test]
    fn caller_owned_call_frames_bound_curried_roots_and_keep_results_alive() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, outer| {
                    let function = language
                        .eval_str(machine, outer, b"let f = x: f; in f", "/")
                        .unwrap();
                    let argument = machine.heap.cell(outer, outer.int(42)).unwrap();
                    let args = vec![argument; 256];
                    let mut warmed_capacity = None;
                    for len in [8, 256, 256] {
                        machine.with_roots(|machine, temp| {
                            let result = language
                                .call_value_in_frame(
                                    machine,
                                    temp,
                                    function,
                                    &args[..len],
                                    SrcPos::NONE,
                                )
                                .unwrap();
                            machine.heap.collect().unwrap();
                            assert!(matches!(
                                machine.heap.view(result),
                                ValueView::Lambda { .. }
                            ));
                        });
                        let capacity = machine.heap.root_slot_bytes();
                        if let Some(warmed) = warmed_capacity {
                            assert_eq!(
                                capacity, warmed,
                                "curried reductions must reuse root frames"
                            );
                        } else {
                            warmed_capacity = Some(capacity);
                        }
                    }
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn caller_owned_callback_frames_preserve_results_errors_and_short_circuiting() {
        let expressions = [
            ("builtins.foldl' (x: y: x + y) 0 [10 20 12]", "42"),
            ("builtins.length (builtins.filter (x: x > 1) [1 2 3])", "2"),
            ("builtins.any (x: x == 1) [1 (throw \"unused\")]", "true"),
            ("builtins.all (x: x == 1) [0 (throw \"unused\")]", "false"),
            ("builtins.head (builtins.sort (x: y: x < y) [3 1 2])", "1"),
            ("builtins.length (builtins.partition (x: x > 1) [1 2 3]).right", "2"),
            ("builtins.length (builtins.groupBy (x: if x > 1 then \"yes\" else \"no\") [1 2 3]).yes", "2"),
            ("builtins.length (builtins.concatMap (x: [x (x + 1)]) [1 2 3])", "6"),
            ("(builtins.tryEval (builtins.any (x: throw \"callback failed\") [1])).success", "false"),
            ("(builtins.tryEval (builtins.foldl' (x: y: throw \"callback failed\") 0 [1])).success", "false"),
        ];
        cases(&expressions, false);
        cases(&expressions, true);
    }

    #[test]
    fn compiled_mixed_flat_sets_preserve_inheritance_literals_and_suspended_variables() {
        for stress in [false, true] {
            cases(&[
                ("let f = x: { a = x; b = 42; c = []; d = throw \"unused\"; }; in (f (throw \"unused variable\")).b", "42"),
                ("let x = 42; in { inherit x; a = []; b = \"literal\"; }.x", "42"),
                ("let a = { b = a; c = []; d = 42; }; in a.b.b.d", "42"),
                ("let f = x: { a = x; b = 2; c = x + 1; }; in (f 40).a + (f 0).b", "42"),
                ("let f = x: { a = ./file; b = \"literal\"; c = x; }; in builtins.baseNameOf (f 1).a", "\"file\""),
                ("builtins.unsafeGetAttrPos \"a\" { a = []; b = 1; } == null", "true"),
            ], stress);
            for len in [1, 64, 65] {
                let mut source = String::from("let f = x: {");
                for i in 0..len {
                    source.push_str(&format!("a{i} = {};", if i % 2 == 0 { "x" } else { "1" }));
                }
                source.push_str("}; in builtins.foldl' builtins.add 0 (builtins.attrValues (f 1))");
                cases(&[(&source, &len.to_string())], stress);
            }
        }
    }

    #[test]
    fn compiled_flat_thunk_sets_preserve_laziness_positions_and_capture() {
        for stress in [false, true] {
            cases(&[
                ("((x: { a = x + 1; b = x + 2; }) 40).b", "42"),
                ("builtins.length (builtins.attrNames { a = throw \"unused\"; b = abort \"unused\"; })", "2"),
                ("let x = { a = x.a; b = 40 + 2; }; in x.b", "42"),
                ("builtins.unsafeGetAttrPos \"a\" { a = 1 + 1; } == null", "true"),
            ], stress);
            for len in [1, 64, 65] {
                let mut source =
                    String::from("builtins.foldl' builtins.add 0 (builtins.attrValues {");
                for i in 0..len {
                    source.push_str(&format!("a{i} = {i} + 1;"));
                }
                source.push_str("})");
                cases(&[(&source, &(len * (len + 1) / 2).to_string())], stress);
            }
        }
    }

    #[test]
    fn compiled_static_sets_preserve_recursive_and_inherited_slots() {
        let mut source =
            String::from("builtins.foldl' builtins.add 0 (builtins.attrValues (rec { a0 = 0;");
        for i in 1..40 {
            source.push_str(&format!("a{i} = a{} + 1;", i - 1));
        }
        source.push_str("}))");
        cases(
            &[
                (&source, "780"),
                (
                    "let x = 20; in (rec { inherit x; a = x; b = a + 22; }).b",
                    "42",
                ),
                ("(rec { inherit ({ y = x + 1; }) y; x = 41; }).y", "42"),
                (
                    "let from = { y = 22; }; x = 20; in { inherit x; inherit (from) y; }.y + x",
                    "42",
                ),
            ],
            true,
        );
    }

    #[test]
    fn initial_library_lambdas_and_lazy_arguments_use_managed_code() {
        cases(
            &[
                ("builtins.length (builtins.map (x: x + 1) [ 1 2 3 ])", "3"),
                ("builtins.foldl' (a: b: a + b) 0 [ 10 12 20 ]", "42"),
                ("let x = throw \"never forced\"; in (y: 42) x", "42"),
                ("(rec { ${\"a\"} = b; b = 42; }).a", "42"),
                ("let x = 42; in (rec { inherit x; y = x; }).y", "42"),
            ],
            true,
        );
    }

    #[test]
    fn scalar_equality_does_not_allocate_operand_cells() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(Evaluator::new(&arena, crate::Config::default(), &host));
        with_owned_heap(brix_gc::Config::default(), |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                language.base_env(machine, roots).unwrap();
                let before = machine.heap.used_bytes();
                for source in [b"42 == 42".as_slice(), b"42 != 1.0"] {
                    let value = language.eval_str(machine, roots, source, "/").unwrap();
                    assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                    assert_eq!(machine.heap.used_bytes(), before);
                }
            });
        })
        .unwrap();
    }

    #[test]
    fn equality_distinguishes_function_values_from_shared_nested_cells() {
        let deep = format!("{}42{}", "[ ".repeat(192), " ]".repeat(192));
        let comparison = format!("{deep} == {deep}");
        cases(
            &[
                ("let f = x: x; in f == f", "false"),
                ("let f = x: x; in f != f", "true"),
                ("builtins.add == builtins.add", "false"),
                ("let f = x: x; in [f] == [f]", "true"),
                ("let f = x: x; in { inherit f; } == { inherit f; }", "true"),
                ("[(x: x)] == [(x: x)]", "false"),
                ("let xs = [xs]; in xs == xs", "true"),
                (&comparison, "true"),
            ],
            true,
        );
    }

    #[test]
    fn equality_fast_paths_preserve_forcing_and_derivation_rules() {
        let expressions = [
                ("let a = { type = \"derivation\"; outPath = throw \"forced\"; }; b = { type = \"derivation\"; outPath = \"x\"; }; in builtins.seq (builtins.tryEval (a == b)) (builtins.tryEval (a == b)).success", "false"),
                ("let a = { type = \"derivation\"; }; b = { type = \"derivation\"; }; in builtins.seq (a == b) (a == b)", "true"),
                ("let a = { type = \"derivation\"; outPath = \"x\"; }; b = { type = \"derivation\"; }; in builtins.seq (a == b) (a == b)", "false"),
                ("let a = { type = \"derivation\"; outPath = \"same\"; }; b = { type = builtins.concatStringsSep \"\" [\"deri\" \"vation\"]; outPath = \"same\"; extra = throw \"unused\"; }; in builtins.seq (a == b) (a == b)", "true"),
                ("let s = builtins.appendContext \"text\" { \"/nix/store/00000000000000000000000000000000-context\" = { path = true; }; }; plain = builtins.unsafeDiscardStringContext s; in s == plain && !(s < plain) && !(plain < s) && [s] == [plain]", "true"),
                ("builtins.deepSeq [42 42.0] ([42] == [42.0])", "true"),
                ("builtins.deepSeq [42 43.0] ([42] == [43.0])", "false"),
                ("let x = true; y = false; in builtins.deepSeq [x y] ([x] == [y])", "false"),
                ("let x = null; y = null; in builtins.deepSeq [x y] ([x] == [y])", "true"),
                ("let x = \"same\"; y = builtins.concatStringsSep \"\" [\"sa\" \"me\"]; in builtins.deepSeq [x y] ([x] == [y])", "true"),
                ("let x = 1; y = \"1\"; in builtins.deepSeq [x y] ([x] == [y])", "false"),
                ("let bad = throw \"forced\"; xs = [bad]; in (builtins.tryEval (xs == xs)).success", "false"),
                ("let bad = throw \"forced\"; s = { a = bad; }; in (builtins.tryEval (s == s)).success", "false"),
                ("let bad = throw \"forced\"; in builtins.seq (builtins.tryEval bad) (builtins.tryEval ([bad] == [bad])).success", "false"),
                ("{ type = \"derivation\"; outPath = \"same\"; } == { type = \"derivation\"; outPath = \"same\"; extra = throw \"unused\"; }", "true"),
                ("let a = { type = builtins.concatStringsSep \"\" [\"deri\" \"vation\"]; outPath = \"same\"; }; b = { type = \"derivation\"; outPath = builtins.concatStringsSep \"\" [\"sa\" \"me\"]; extra = throw \"unused\"; }; in a == b", "true"),
                ("{ type = \"derivation\"; outPath = \"a\"; } == { type = \"derivation\"; outPath = \"b\"; }", "false"),
                ("{ type = 42; a = 1; } == { type = 42; a = 1; }", "true"),
                ("{ type = \"ordinary\"; } == { type = \"ordinary\"; extra = 1; }", "false"),
                ("(builtins.tryEval ({ type = throw \"forced\"; } == {})).success", "false"),
                ("(builtins.tryEval ({ a = 1; } == { type = throw \"unused\"; })).success", "true"),
            ];
        for stress in [false, true] {
            cases(&expressions, stress);
        }
    }

    #[test]
    fn borrowed_elem_preserves_order_identity_and_cached_derivation_rules() {
        let expressions = [
            ("builtins.elem (throw \"unused\") []", "false"),
            ("builtins.elem 1 [1 (throw \"unused\")]", "true"),
            ("(builtins.tryEval (builtins.elem 1 [(throw \"first\") 1])).success", "false"),
            ("let bad = throw \"failed\"; in builtins.seq (builtins.tryEval bad) (builtins.tryEval (builtins.elem bad [bad])).success", "false"),
            ("let f = x: x; in builtins.seq f (builtins.elem f [f])", "true"),
            ("let f = x: x; g = x: x; in builtins.deepSeq [f g] (builtins.elem f [g])", "false"),
            ("let a = { type = \"derivation\"; outPath = \"same\"; }; b = { type = \"derivation\"; outPath = \"same\"; extra = throw \"unused\"; }; in builtins.seq (a == b) (builtins.elem a [b])", "true"),
            ("let a = { type = \"derivation\"; outPath = \"a\"; }; b = { type = \"derivation\"; outPath = \"b\"; }; in builtins.seq (a == b) (builtins.elem a [b a])", "true"),
            ("let a = { type = \"derivation\"; outPath = throw \"failed output\"; }; b = { type = \"derivation\"; outPath = \"same\"; }; in builtins.seq (builtins.tryEval (a == b)) (builtins.tryEval (builtins.elem a [b])).success", "false"),
            ("let f = x: x; a = { type = \"derivation\"; outPath = f; }; b = { type = \"derivation\"; outPath = f; extra = throw \"unused\"; }; in builtins.seq (a == b) (builtins.elem a [b])", "true"),
            ("let a = { type = \"derivation\"; outPath = [1 2]; }; b = { type = \"derivation\"; outPath = [1 2]; }; in builtins.seq (a == b) (builtins.elem a [b])", "true"),
        ];
        for stress in [false, true] {
            cases(&expressions, stress);
        }
    }

    #[test]
    fn cached_derivation_equality_keeps_nested_depth_checks() {
        use crate::managed_builtins::{equal, equal_if_ready};
        for max_call_depth in [0, 1] {
            let arena = crate::Arena::new();
            let host = crate::NullHost;
            let config = crate::Config {
                max_call_depth,
                ..crate::Config::default()
            };
            let language = Language::new(Evaluator::new(&arena, config, &host));
            with_owned_heap(brix_gc::Config::default(), |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let kind = machine.heap.string(roots, b"derivation").unwrap();
                    let kind = machine.heap.cell(roots, kind).unwrap();
                    let output = machine.heap.cell(roots, roots.int(42)).unwrap();
                    let set = machine
                        .heap
                        .alloc_flat_bindings(
                            roots,
                            SrcPos::NONE,
                            &[
                                RootedAttr {
                                    name: sym::OUT_PATH,
                                    pos: SrcPos::NONE,
                                    value: output,
                                },
                                RootedAttr {
                                    name: sym::TYPE,
                                    pos: SrcPos::NONE,
                                    value: kind,
                                },
                            ],
                        )
                        .unwrap();
                    let value = machine.heap.bindings_value(roots, set);
                    // Distinct operand cells share a set and its output cell.
                    // The nested depth check must precede output identity.
                    let cells = [
                        machine.heap.cell(roots, value).unwrap(),
                        machine.heap.cell(roots, value).unwrap(),
                    ];
                    // A cold probe falls back; it must not retain its guard.
                    assert_eq!(
                        equal_if_ready(
                            &language,
                            &machine.heap,
                            machine.heap.cell_view(cells[0]),
                            machine.heap.cell_view(cells[1]),
                            SrcPos::NONE,
                        )
                        .unwrap(),
                        None
                    );
                    machine.heap.cache_derivation(set, true);
                    for _ in 0..2 {
                        let result = equal_if_ready(
                            &language,
                            &machine.heap,
                            machine.heap.cell_view(cells[0]),
                            machine.heap.cell_view(cells[1]),
                            SrcPos::NONE,
                        );
                        if max_call_depth == 0 {
                            assert_eq!(result.unwrap_err().kind, ErrorKind::StackOverflow);
                        } else {
                            assert_eq!(result.unwrap(), Some(true));
                        }
                    }
                    // Invalidating the cache restores the full comparison,
                    // which must impose the same nested output depth check.
                    machine.heap.collect().unwrap();
                    let result = equal(&language, machine, roots, cells[0], cells[1], SrcPos::NONE);
                    if max_call_depth == 0 {
                        assert_eq!(result.unwrap_err().kind, ErrorKind::StackOverflow);
                    } else {
                        assert!(result.unwrap());
                    }
                });
            })
            .unwrap();
        }
    }

    #[test]
    fn cached_builtin_handlers_preserve_aliases_and_callback_overrides() {
        use crate::managed_api::{GcEvaluator, GcValueView};
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let mut evaluator = GcEvaluator::new(
            legacy,
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
        )
        .unwrap();
        // A callback overrides a recognized native handler, through both names.
        evaluator
            .register_callback("__add", |context, _, _| context.int(73))
            .unwrap();
        for source in [b"builtins.add 1 2".as_slice(), b"__add 1 2"] {
            let value = evaluator.eval_str(source, b"/").unwrap();
            assert!(matches!(
                evaluator.view(&value).unwrap(),
                GcValueView::Int(73)
            ));
        }
        let value = evaluator.eval_str(
            b"builtins.sub 9 2 == __sub 9 2 && builtins.stringLength \"abc\" == __stringLength \"abc\"",
            b"/",
        ).unwrap();
        assert!(matches!(
            evaluator.view(&value).unwrap(),
            GcValueView::Bool(true)
        ));
    }

    #[test]
    fn language_errors_preserve_legacy_positions_and_traces() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = Evaluator::new(
            &arena,
            crate::Config {
                show_trace: true,
                ..Default::default()
            },
            &host,
        );
        let language = Language::new(legacy);
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                chunk_bytes: 256,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                for source in [
                    "(x: y: x + y) 1 false",
                    "builtins.seq 0 (x: x.nope) {}",
                    "let f = builtins.add 1; in f false",
                    "__add 1 false",
                    "__stringLength 1",
                    "__head []",
                    "__baseNameOf {}",
                    "({ __functor = self: x: y: x + y; }) 1 false",
                    "if 1 then 2 else 3",
                    "({ x }: x) {}",
                    "({ x }: x) { y = 2; }",
                    "let x = x; in x",
                    "(x: x) (throw \"level zero\")",
                    "(x: y: x) (throw \"level one\") 0",
                    "(x: y: z: x) (throw \"deep level\") 0 0",
                    "let x = x; in (a: b: x) 1 2",
                    "({ x ? y, y ? x }: x) {}",
                    "{ x = 1; }.y",
                    "{ abc = 1; }.abd",
                    "(1).x",
                    "{ x = throw \"selected\"; }.x",
                    "{ \"quoted name\" = throw \"selected\"; }.\"quoted name\"",
                    "{ x = throw \"default does not hide this\"; }.x or 42",
                    "with {}; missing",
                    "(x: x.nope) {}",
                    "assert false; 1",
                    "assert (1 == 2); null",
                    "assert (1 == true); null",
                    "assert ([1 2] == [1 3]); null",
                    "assert ([1] == [1 2]); null",
                    "assert ({x = 1;} == {x = 2;}); null",
                    "assert ({x = 1;} == {y = 1;}); null",
                    "assert ((x: x) == (x: x)); null",
                ] {
                    let expected = language
                        .legacy()
                        .eval_str(source.as_bytes(), "/")
                        .unwrap_err()
                        .render(true);
                    machine.with_roots(|machine, roots| {
                        let actual = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .unwrap_err()
                            .render(true);
                        assert_eq!(actual, expected, "{source}");
                    });
                }
            },
        )
        .unwrap();
    }
}
