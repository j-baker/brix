//! Root-aware builtin dispatch and shared forcing/coercion operations.
#![allow(dead_code)]

use crate::error::{msg, ErrorKind};
use crate::managed_builtins_strings::{self as strings, OwnedContext};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{BindingsRoot, CellRoot, Roots, ValueRoot, ValueView};
use crate::managed_language::Language;
use crate::symbol::sym;
use crate::value::SrcPos;
use crate::Result;

pub(crate) fn show_type(value: ValueView<'_, '_>) -> &'static str {
    match value {
        ValueView::Int(_) => "an integer",
        ValueView::Float(_) => "a float",
        ValueView::Bool(_) => "a Boolean",
        ValueView::Null => "null",
        ValueView::String(s)
            if s.contexts().any(|c| {
                matches!(
                    c,
                    crate::managed_heap::ContextView::Opaque(_)
                        | crate::managed_heap::ContextView::DrvDeep(_)
                        | crate::managed_heap::ContextView::Built { .. }
                )
            }) =>
        {
            "a string with context"
        }
        ValueView::String(_) => "a string",
        ValueView::Path(_) => "a path",
        ValueView::Attrs(_) => "a set",
        ValueView::List(_) => "a list",
        ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => "a function",
        ValueView::Blackhole => "a black hole",
        ValueView::App(_) => "a function application",
        ValueView::Thunk { .. } | ValueView::Failed(_) => "a thunk",
    }
}

pub(crate) fn print_for_error<'h>(
    language: &Language<'_>,
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
) -> Vec<u8> {
    crate::managed_print::print_for_error(language, machine, value)
}

pub(crate) fn type_error<'h>(
    language: &Language<'_>,
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
    expected: &str,
    pos: SrcPos,
) -> crate::Error {
    language.legacy().error_at(
        ErrorKind::TypeError,
        msg!(
            "expected ",
            expected,
            " but found ",
            crate::managed_print::describe_type(language, machine, value),
            ": ",
            print_for_error(language, machine, value)
        ),
        pos,
    )
}

macro_rules! force_scalar {
    ($name:ident, $ty:ty, $expected:literal, $($pat:pat => $result:expr),+ $(,)?) => {
        pub(crate) fn $name<'r,'h>(language:&Language<'_>,machine:&mut Machine<'h>,roots:&Roots<'r,'h>,cell:CellRoot<'_,'h>,pos:SrcPos,ctx:&str)->Result<$ty> {
            // Already-forced scalar payloads contain no relocatable edges.
            // Reading them needs neither a snapshot root nor a new frame.
            match machine.heap.read(cell) { $($pat => return Ok($result),)+ _=>{} }
            let result = (|| { language.force_cell(machine,roots,cell,pos)?;
                match machine.heap.read(cell) { $($pat => Ok($result),)+ _=>Err(type_error(language,machine,machine.heap.value(roots,cell),$expected,pos)) }
            })();
            result.map_err(|e| e.with_trace(language.legacy().position(pos),ctx))
        }
    }
}
force_scalar!(force_int,i64,"an integer",ValueView::Int(i)=>i);
force_scalar!(force_float,f64,"a float",ValueView::Int(i)=>i as f64,ValueView::Float(f)=>f);
force_scalar!(force_bool,bool,"a Boolean",ValueView::Bool(b)=>b);

pub(crate) fn force_attrs<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<BindingsRoot<'r, 'h>> {
    language.force_cell(machine, roots, cell, pos)?;
    match machine.heap.read(cell) {
        ValueView::Attrs(b) => Ok(b.root(roots)),
        _ => Err(type_error(
            language,
            machine,
            machine.heap.value(roots, cell),
            "a set",
            SrcPos::NONE,
        )
        .with_trace(language.legacy().position(pos), ctx)),
    }
}
pub(crate) fn force_list<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let v = language.force(machine, roots, cell, pos)?;
    match machine.heap.view(v) {
        ValueView::List(_) => Ok(v),
        _ => Err(type_error(language, machine, v, "a list", SrcPos::NONE)
            .with_trace(language.legacy().position(pos), ctx)),
    }
}
pub(crate) fn force_function<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let result = (|| {
        let v = language.force(machine, roots, cell, pos)?;
        let callable = match machine.heap.view(v) {
            ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => true,
            ValueView::Attrs(b) => b.get(sym::FUNCTOR).is_some(),
            _ => false,
        };
        if callable {
            Ok(v)
        } else {
            Err(type_error(language, machine, v, "a function", pos))
        }
    })();
    result.map_err(|e| e.with_trace(language.legacy().position(pos), ctx))
}
pub(crate) fn force_string<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let result = (|| {
        let v = language.force(machine, roots, cell, pos)?;
        match machine.heap.view(v) {
            ValueView::String(_) => Ok(v),
            _ => Err(type_error(language, machine, v, "a string", pos)),
        }
    })();
    result.map_err(|e| e.with_trace(language.legacy().position(pos), ctx))
}
/// Force and materialize a string, validating real context without copying it.
/// The returned root can cross collection; borrow its bytes only when consuming.
pub(crate) fn force_string_no_ctx_value<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let v = force_string(language, machine, roots, cell, pos, ctx)?;
    let v = strings::materialize(language, machine, roots, v)?;
    let ValueView::String(string) = machine.heap.view(v) else {
        unreachable!("force_string returned a string")
    };
    // Only diagnostics need owned context bytes. Inspect the original order
    // directly while the heap is immutably borrowed; no collection can overlap.
    for c in string.contexts() {
        let dir = language.legacy().config.store_dir.as_str();
        let display = match c {
            crate::managed_heap::ContextView::Opaque(p) => msg!(dir, "/", p),
            crate::managed_heap::ContextView::DrvDeep(p) => msg!(dir, "/", p, " (deep)"),
            crate::managed_heap::ContextView::Built { drv, output } => {
                msg!(dir, "/", drv, "^", output)
            }
            _ => continue,
        };
        return Err(language
            .legacy()
            .error(
                ErrorKind::Eval,
                msg!(
                    "the string '",
                    string.bytes(),
                    "' is not allowed to refer to a store path (such as '",
                    display,
                    "')"
                ),
            )
            .with_trace(language.legacy().position(pos), ctx));
    }
    Ok(v)
}

/// Own bytes only for callers that must retain them across managed mutations.
pub(crate) fn force_string_no_ctx<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<Vec<u8>> {
    let value = force_string_no_ctx_value(language, machine, roots, cell, pos, ctx)?;
    Ok(strings::bytes(machine, value).to_vec())
}

pub(crate) fn coerce<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    more: bool,
    copy_to_store: bool,
) -> Result<ValueRoot<'r, 'h>> {
    coerce_mode(
        language,
        machine,
        out,
        cell,
        pos,
        crate::coerce::Coercion {
            more,
            copy_to_store,
            canonicalize: true,
        },
    )
}

pub(crate) fn coerce_mode<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    mode: crate::coerce::Coercion,
) -> Result<ValueRoot<'r, 'h>> {
    coerce_with_context(language, machine, out, cell, pos, mode, "")
}

pub(crate) fn coerce_with_context<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    mode: crate::coerce::Coercion,
    ctx: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let crate::coerce::Coercion {
        more,
        copy_to_store,
        canonicalize,
    } = mode;
    let _guard = language.legacy().enter(pos)?;
    out.with_roots(|roots| {
        let value = language.force(machine, roots, cell, pos)?;
        match machine.heap.view(value) {
            ValueView::String(_) => Ok(out.copy(value)),
            ValueView::Path(p) => {
                let ev = language.legacy();
                let accessor = p.accessor();
                let bytes = p.bytes().to_vec();
                if !copy_to_store && accessor == ev.root_accessor {
                    return crate::managed_builtins_paths::lazy_path_string(
                        language,
                        machine,
                        out,
                        &bytes,
                        canonicalize,
                    );
                }
                let p = crate::value::PathVal {
                    accessor,
                    path: &bytes,
                };
                let mut context = crate::string_context::StringContext::new();
                let bytes = if copy_to_store {
                    let store = ev.copy_path_to_store(&mut context, &p, pos)?;
                    ev.config.store_dir.display(&store).as_bytes().to_vec()
                } else {
                    let text = ev.path_text(&p)?.into_owned();
                    if accessor == ev.root_accessor {
                        if let Some(store) = ev.maybe_store_path(&text) {
                            return strings::string(
                                machine,
                                out,
                                &text,
                                &[OwnedContext::Path(store.as_str().as_bytes().to_vec())],
                            );
                        }
                    }
                    text
                };
                let owned: Vec<_> = context
                    .as_slice()
                    .iter()
                    .map(|c| match c {
                        crate::value::ContextElem::LazyHash { offset, tree } => {
                            OwnedContext::LazyHash {
                                offset: *offset,
                                tree: *tree,
                            }
                        }
                        crate::value::ContextElem::LazyPath(t) => OwnedContext::LazyPath(*t),
                        crate::value::ContextElem::Opaque(p) => {
                            OwnedContext::Opaque(p.as_bytes().to_vec())
                        }
                        crate::value::ContextElem::DrvDeep(p) => {
                            OwnedContext::DrvDeep(p.as_bytes().to_vec())
                        }
                        crate::value::ContextElem::Path(p) => {
                            OwnedContext::Path(p.as_bytes().to_vec())
                        }
                        crate::value::ContextElem::Built { drv, output } => OwnedContext::Built {
                            drv: drv.as_bytes().to_vec(),
                            output: output.to_vec(),
                        },
                    })
                    .collect();
                strings::string(machine, out, &bytes, &owned)
            }
            ValueView::Attrs(b) => {
                let ts = b.get(sym::TO_STRING).map(|a| a.value.root(roots));
                let op = b.get(sym::OUT_PATH).map(|a| a.value.root(roots));
                if let Some(ts) = ts {
                    let f = language.force(machine, roots, ts, pos)?;
                    let result = language.call_value(machine, roots, f, &[cell], pos)?;
                    let result = allocation(machine.heap.cell(roots, result))?;
                    coerce_with_context(
                        language,
                        machine,
                        out,
                        result,
                        pos,
                        crate::coerce::Coercion {
                            canonicalize: true,
                            ..mode
                        },
                        "while evaluating the result of the `__toString` attribute",
                    )
                } else if let Some(op) = op {
                    coerce_with_context(language, machine, out, op, pos, mode, ctx)
                } else {
                    Err(language
                        .legacy()
                        .error(
                            ErrorKind::TypeError,
                            msg!(
                                "cannot coerce ",
                                show_type(machine.heap.view(value)),
                                " to a string: ",
                                print_for_error(language, machine, value)
                            ),
                        )
                        .with_trace(language.legacy().position(pos), ctx))
                }
            }
            ValueView::List(list) if more => {
                let n = list.len();
                let mut bytes = Vec::new();
                let mut context = Vec::new();
                for i in 0..n {
                    roots.with_roots(|temp| {
                        let ValueView::List(list) = machine.heap.view(value) else {
                            unreachable!()
                        };
                        let cell = list.get(i).unwrap().root(temp);
                        let part = stacker::maybe_grow(
                            crate::stack::STACK_RED_ZONE,
                            crate::stack::STACK_SEGMENT,
                            || {
                                coerce_with_context(
                                    language,
                                    machine,
                                    temp,
                                    cell,
                                    pos,
                                    mode,
                                    "while evaluating one element of the list",
                                )
                            },
                        )
                        .inspect_err(|e| language.legacy().add_trace(e, pos, ctx))?;
                        strings::append_snapshot(machine, part, &mut bytes, &mut context)?;
                        if i + 1 < n
                            && !matches!(machine.heap.read(cell),ValueView::List(l) if l.is_empty())
                        {
                            bytes.push(b' ');
                        }
                        Ok::<_, crate::Error>(())
                    })?;
                }
                strings::string(machine, out, &bytes, &context)
            }
            ValueView::Bool(b) if more => {
                allocation(machine.heap.string(out, if b { b"1" } else { b"" }))
            }
            ValueView::Null if more => allocation(machine.heap.string(out, b"")),
            ValueView::Int(i) if more => {
                allocation(machine.heap.string(out, i.to_string().as_bytes()))
            }
            ValueView::Float(f) if more => allocation(
                machine
                    .heap
                    .string(out, crate::coerce::float_to_string(f).as_bytes()),
            ),
            _ => Err(language
                .legacy()
                .error(
                    ErrorKind::TypeError,
                    msg!(
                        "cannot coerce ",
                        show_type(machine.heap.view(value)),
                        " to a string: ",
                        print_for_error(language, machine, value)
                    ),
                )
                .with_trace(language.legacy().position(pos), ctx)),
        }
    })
}

/// Compare cells with Nix's identity shortcut, after forcing both operands.
/// Nested list/attribute comparisons use this entry so shared function cells
/// compare equal, even though two function values themselves do not.
#[inline]
pub(crate) fn equal<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    left: CellRoot<'_, 'h>,
    right: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    let _guard = language.legacy().enter(pos)?;
    if let (Some(a), Some(b)) = (machine.heap.read_whnf(left), machine.heap.read_whnf(right)) {
        // The caller's cell roots already keep both values alive. Borrow them
        // for comparisons that cannot collect, without a frame or snapshots.
        if machine.heap.same_cell(left, right) {
            return Ok(true);
        }
        if let Some(equal) = equal_known(language, &machine.heap, a, b, pos)? {
            return Ok(equal);
        }
        return equal_ready(language, machine, out, left, right, pos);
    }
    equal_slow(language, machine, out, left, right, pos)
}

/// Both cells are already evaluated and the identity/scalar shortcuts have
/// been checked. Avoid repeating forcing and borrowed comparison on this path.
#[inline(never)]
fn equal_ready<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'_, 'h>,
    left: CellRoot<'_, 'h>,
    right: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    language.legacy().with_stack(|| {
        out.with_roots(|roots| {
            let a = machine.heap.value(roots, left);
            let b = machine.heap.value(roots, right);
            equal_forced(language, machine, roots, a, b, pos)
        })
    })
}

// Keep forcing, snapshots and recursive comparisons out of the scalar path.
// The caller's depth guard stays alive through this call.
#[inline(never)]
fn equal_slow<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'_, 'h>,
    left: CellRoot<'_, 'h>,
    right: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    language.legacy().with_stack(|| {
        out.with_roots(|roots| {
            // Force in the original left-to-right order. Identity is only a
            // shortcut after both operands have exposed errors/black holes.
            language.force_cell(machine, roots, left, pos)?;
            language.force_cell(machine, roots, right, pos)?;
            if machine.heap.same_cell(left, right) {
                return Ok(true);
            }
            if let Some(equal) = equal_known(
                language,
                &machine.heap,
                machine.heap.read(left),
                machine.heap.read(right),
                pos,
            )? {
                return Ok(equal);
            }
            let a = machine.heap.value(roots, left);
            let b = machine.heap.value(roots, right);
            equal_forced(language, machine, roots, a, b, pos)
        })
    })
}

/// Probe a comparison without publishing roots or allowing collection. A miss
/// must use the ordinary equality path, in the original evaluation order.
pub(crate) enum EqualityScan {
    Found,
    Exhausted,
    Fallback(usize),
}

/// A target prepared for a contiguous scan that cannot allocate in the moving
/// heap or collect.
/// No suspended value is forced during preparation, including its type/output.
struct PreparedEquality<'v, 'h> {
    cell: crate::managed_heap::CellView<'v, 'h>,
    value: ValueView<'v, 'h>,
    output: Option<(crate::managed_heap::CellView<'v, 'h>, ValueView<'v, 'h>)>,
}

impl<'v, 'h> PreparedEquality<'v, 'h> {
    #[inline]
    fn new(
        heap: &'v crate::managed_heap::Heap<'h>,
        cell: crate::managed_heap::CellView<'v, 'h>,
    ) -> Option<Self> {
        if !heap.is_whnf_view(cell) {
            return None;
        }
        let value = heap.read_view(cell);
        let output = match value {
            ValueView::Attrs(set) => heap
                .cached_derivation_output(set)
                .filter(|cell| heap.is_whnf_view(*cell))
                .map(|cell| (cell, heap.read_view(cell))),
            _ => None,
        };
        Some(Self {
            cell,
            value,
            output,
        })
    }

    #[inline]
    fn compare(
        &self,
        language: &Language<'_>,
        heap: &crate::managed_heap::Heap<'h>,
        right: crate::managed_heap::CellView<'_, 'h>,
        pos: SrcPos,
    ) -> Result<Option<bool>> {
        if !heap.is_whnf_view(right) {
            return Ok(None);
        }
        let _guard = language.legacy().enter(pos)?;
        if heap.same_cell_views(self.cell, right) {
            return Ok(Some(true));
        }
        let right = heap.read_view(right);
        if let (ValueView::Attrs(_), ValueView::Attrs(set)) = (self.value, right) {
            let Some((left_cell, left_value)) = self.output else {
                return Ok(None);
            };
            let Some(right_cell) = heap.cached_derivation_output(set) else {
                return Ok(None);
            };
            if !heap.is_whnf_view(right_cell) {
                return Ok(None);
            }
            // Preserve the additional output-comparison depth, before identity.
            let _output_guard = language.legacy().enter(pos)?;
            if heap.same_cell_views(left_cell, right_cell) {
                return Ok(Some(true));
            }
            Ok(equal_borrowed(left_value, heap.read_view(right_cell)))
        } else {
            Ok(equal_borrowed(self.value, right))
        }
    }
}

/// Stop at the first comparison requiring evaluation. The result contains no
/// borrowed pointers, so the caller may force/collect then prepare a new scan.
pub(crate) fn scan_equal_ready<'h>(
    language: &Language<'_>,
    heap: &crate::managed_heap::Heap<'h>,
    target: crate::managed_heap::CellView<'_, 'h>,
    items: crate::managed_heap::ListView<'_, 'h>,
    start: usize,
    pos: SrcPos,
) -> Result<EqualityScan> {
    if start >= items.len() {
        return Ok(EqualityScan::Exhausted);
    }
    let Some(target) = PreparedEquality::new(heap, target) else {
        return Ok(EqualityScan::Fallback(start));
    };
    for index in start..items.len() {
        match target.compare(language, heap, items.get(index).unwrap(), pos)? {
            Some(true) => return Ok(EqualityScan::Found),
            Some(false) => {}
            None => return Ok(EqualityScan::Fallback(index)),
        }
    }
    Ok(EqualityScan::Exhausted)
}

#[inline]
pub(crate) fn equal_if_ready<'h>(
    language: &Language<'_>,
    heap: &crate::managed_heap::Heap<'h>,
    left: crate::managed_heap::CellView<'_, 'h>,
    right: crate::managed_heap::CellView<'_, 'h>,
    pos: SrcPos,
) -> Result<Option<bool>> {
    match PreparedEquality::new(heap, left) {
        Some(target) => target.compare(language, heap, right, pos),
        None => Ok(None),
    }
}

#[inline]
fn equal_known<'h>(
    language: &Language<'_>,
    heap: &crate::managed_heap::Heap<'h>,
    left: ValueView<'_, 'h>,
    right: ValueView<'_, 'h>,
    pos: SrcPos,
) -> Result<Option<bool>> {
    if let (ValueView::Attrs(x), ValueView::Attrs(y)) = (left, right) {
        equal_cached_derivations(language, heap, x, y, pos)
    } else {
        Ok(equal_borrowed(left, right))
    }
}

#[inline]
fn equal_cached_derivations<'h>(
    language: &Language<'_>,
    heap: &crate::managed_heap::Heap<'h>,
    left: crate::managed_heap::BindingsView<'_, 'h>,
    right: crate::managed_heap::BindingsView<'_, 'h>,
    pos: SrcPos,
) -> Result<Option<bool>> {
    let (Some(x), Some(y)) = (
        heap.cached_derivation_output(left),
        heap.cached_derivation_output(right),
    ) else {
        return Ok(None);
    };
    if !heap.is_whnf_view(x) || !heap.is_whnf_view(y) {
        return Ok(None);
    }
    // The general derivation path calls equal recursively for the outputs.
    // Retain that depth check, as well as its forced-cell identity shortcut.
    let _guard = language.legacy().enter(pos)?;
    if heap.same_cell_views(x, y) {
        return Ok(Some(true));
    }
    Ok(equal_borrowed(heap.read_view(x), heap.read_view(y)))
}

/// Both inputs must already be in WHNF. Recursive comparisons and paths keep
/// their existing fallback; lazy string hashes may require materialisation.
#[inline]
fn equal_borrowed<'h>(left: ValueView<'_, 'h>, right: ValueView<'_, 'h>) -> Option<bool> {
    Some(match (left, right) {
        (ValueView::Int(x), ValueView::Int(y)) => x == y,
        (ValueView::Int(x), ValueView::Float(y)) => x as f64 == y,
        (ValueView::Float(x), ValueView::Int(y)) => x == y as f64,
        (ValueView::Float(x), ValueView::Float(y)) => x == y,
        (ValueView::Bool(x), ValueView::Bool(y)) => x == y,
        (ValueView::Null, ValueView::Null) => true,
        (ValueView::String(x), ValueView::String(y)) => {
            return strings::known_view_comparison(x, y, false);
        }
        (ValueView::List(_), ValueView::List(_))
        | (ValueView::Attrs(_), ValueView::Attrs(_))
        | (ValueView::Path(_), ValueView::Path(_)) => return None,
        _ => false,
    })
}

/// Inspect a borrowed type without creating roots. A lazy hash retains the
/// materialising path; a suspended or failed cell retains the forcing path.
#[inline]
fn known_derivation_type(value: ValueView<'_, '_>) -> Option<bool> {
    match value {
        ValueView::String(s) => {
            if s.has_lazy_hash() {
                None
            } else {
                Some(s.bytes() == b"derivation")
            }
        }
        ValueView::Thunk { .. }
        | ValueView::App(_)
        | ValueView::Blackhole
        | ValueView::Failed(_) => None,
        _ => Some(false),
    }
}

/// The common case is a completed classification from this GC epoch. Keep
/// forcing and materialisation out of line so repeated probes stay small.
#[inline]
fn is_derivation<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    set: BindingsRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    if let Some(value) = machine.heap.cached_derivation(set) {
        #[cfg(feature = "work-counters")]
        crate::stats::Stats::bump_work(&language.legacy().stats().derivation_cache_hits);
        return Ok(value);
    }
    classify_derivation(language, machine, roots, set, pos)
}

#[cold]
#[inline(never)]
fn classify_derivation<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    set: BindingsRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    let attr = machine.heap.bindings_view(set).get(sym::TYPE);
    #[cfg(feature = "work-counters")]
    machine
        .heap
        .profile_derivation_probe(language.legacy().stats(), set, attr.map(|a| a.value));
    let Some(attr) = attr else {
        machine.heap.cache_derivation(set, false);
        return Ok(false);
    };
    if let Some(value) = known_derivation_type(machine.heap.read_view(attr.value)) {
        machine.heap.cache_derivation(set, value);
        return Ok(value);
    }
    let t = attr.value.root(roots);
    language.force_cell(machine, roots, t, pos)?;
    if let Some(value) = known_derivation_type(machine.heap.read(t)) {
        // Forcing may have collected: both the key and epoch are read afresh.
        machine.heap.cache_derivation(set, value);
        return Ok(value);
    }
    let t = machine.heap.value(roots, t);
    let t = strings::materialize(language, machine, roots, t)?;
    Ok(matches!(machine.heap.view(t), ValueView::String(s) if s.bytes() == b"derivation"))
}

/// Compare temporary values without allocating heap cells when already forced.
/// There is deliberately no identity shortcut at this level: `f == f` is
/// false for a function, while `[f] == [f]` compares shared element cells.
pub(crate) fn equal_values<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    left: ValueRoot<'_, 'h>,
    right: ValueRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    let suspended = |value| {
        matches!(
            value,
            ValueView::Thunk { .. }
                | ValueView::App(_)
                | ValueView::Blackhole
                | ValueView::Failed(_)
        )
    };
    // Native callbacks may return an unforced value. Keep the legacy fallback
    // for that case; two newly allocated cells must not gain an identity match.
    if suspended(machine.heap.view(left)) || suspended(machine.heap.view(right)) {
        let left = allocation(machine.heap.cell(roots, left))?;
        let right = allocation(machine.heap.cell(roots, right))?;
        return equal(language, machine, roots, left, right, pos);
    }
    let _guard = language.legacy().enter(pos)?;
    equal_forced(language, machine, roots, left, right, pos)
}

fn equal_forced<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    a: ValueRoot<'_, 'h>,
    b: ValueRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<bool> {
    Ok(match (machine.heap.view(a), machine.heap.view(b)) {
        (ValueView::Int(x), ValueView::Int(y)) => x == y,
        (ValueView::Int(x), ValueView::Float(y)) => x as f64 == y,
        (ValueView::Float(x), ValueView::Int(y)) => x == y as f64,
        (ValueView::Float(x), ValueView::Float(y)) => x == y,
        (ValueView::Bool(x), ValueView::Bool(y)) => x == y,
        (ValueView::Null, ValueView::Null) => true,
        (ValueView::String(_), ValueView::String(_)) => {
            if let Some(equal) = strings::known_comparison(machine, a, b, false) {
                return Ok(equal);
            }
            let a = strings::materialize(language, machine, roots, a)?;
            let b = strings::materialize(language, machine, roots, b)?;
            let (ValueView::String(x), ValueView::String(y)) =
                (machine.heap.view(a), machine.heap.view(b))
            else {
                unreachable!()
            };
            x.bytes() == y.bytes()
        }
        (ValueView::Path(x), ValueView::Path(y)) => {
            x.accessor() == y.accessor()
                && if x.accessor() == language.legacy().root_accessor {
                    language.legacy().root_paths_equal(x.bytes(), y.bytes())?
                } else {
                    x.bytes() == y.bytes()
                }
        }
        (ValueView::List(x), ValueView::List(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            let len = x.len();
            for i in 0..len {
                let same = roots.with_roots(|temp| {
                    let (ValueView::List(x), ValueView::List(y)) =
                        (machine.heap.view(a), machine.heap.view(b))
                    else {
                        unreachable!()
                    };
                    let x = x.get(i).unwrap().root(temp);
                    let y = y.get(i).unwrap().root(temp);
                    equal(language, machine, temp, x, y, pos)
                })?;
                if !same {
                    return Ok(false);
                }
            }
            true
        }
        (ValueView::Attrs(x), ValueView::Attrs(y)) => {
            if let Some(equal) = equal_cached_derivations(language, &machine.heap, x, y, pos)? {
                return Ok(equal);
            }
            let x = x.root(roots);
            let y = y.root(roots);
            if is_derivation(language, machine, roots, x, pos)?
                && is_derivation(language, machine, roots, y, pos)?
            {
                let op = machine
                    .heap
                    .derivation_out_path(x)
                    .map(|cell| cell.root(roots));
                let oq = machine
                    .heap
                    .derivation_out_path(y)
                    .map(|cell| cell.root(roots));
                if let (Some(p), Some(q)) = (op, oq) {
                    return equal(language, machine, roots, p, q, pos);
                }
            }
            let xv = machine.heap.bindings_view(x);
            let yv = machine.heap.bindings_view(y);
            if xv.len() != yv.len() {
                return Ok(false);
            }
            let mut remaining = xv.len();
            let mut after = None;
            while remaining != 0 {
                // Root at most 32 pairs, then discard all comparison temporaries.
                // Names and order stay stable even when forcing moves the graph.
                let same = roots.with_roots(|temp| -> Result<bool> {
                    let pairs = machine.heap.bindings_view(x).comparison_batch(
                        machine.heap.bindings_view(y),
                        temp,
                        after,
                    );
                    assert!(
                        !pairs.is_empty(),
                        "equal-sized immutable sets have remaining pairs"
                    );
                    remaining -= pairs.len();
                    for (name, other_name, p, q) in pairs {
                        if name != other_name || !equal(language, machine, temp, p, q, pos)? {
                            return Ok(false);
                        }
                        after = Some((name, other_name));
                    }
                    Ok(true)
                })?;
                if !same {
                    return Ok(false);
                }
            }
            true
        }
        _ => false,
    })
}

pub(crate) fn compare<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    left: CellRoot<'_, 'h>,
    right: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<bool> {
    let _guard = language.legacy().enter(pos)?;
    let result = out.with_roots(|roots| {
        let a = language.force(machine, roots, left, pos)?;
        let b = language.force(machine, roots, right, pos)?;
        match (machine.heap.view(a), machine.heap.view(b)) {
            (ValueView::Int(x), ValueView::Int(y)) => Ok(x < y),
            (ValueView::Int(x), ValueView::Float(y)) => Ok((x as f64) < y),
            (ValueView::Float(x), ValueView::Int(y)) => Ok(x < (y as f64)),
            (ValueView::Float(x), ValueView::Float(y)) => Ok(x < y),
            (ValueView::String(_), ValueView::String(_)) => {
                if let Some(less) = strings::known_comparison(machine, a, b, true) {
                    return Ok(less);
                }
                let a = strings::materialize(language, machine, roots, a)?;
                let b = strings::materialize(language, machine, roots, b)?;
                let (ValueView::String(x), ValueView::String(y)) =
                    (machine.heap.view(a), machine.heap.view(b))
                else {
                    unreachable!()
                };
                Ok(x.bytes() < y.bytes())
            }
            (ValueView::Path(x), ValueView::Path(y)) => {
                if x.accessor() == language.legacy().root_accessor
                    && y.accessor() == language.legacy().root_accessor
                {
                    language.legacy().root_path_less(x.bytes(), y.bytes())
                } else {
                    Ok(x.bytes() < y.bytes())
                }
            }
            (ValueView::List(x), ValueView::List(y)) => {
                let (nx, ny) = (x.len(), y.len());
                for i in 0..nx.min(ny) {
                    let cmp = roots.with_roots(|temp| {
                        let (ValueView::List(x), ValueView::List(y)) =
                            (machine.heap.view(a), machine.heap.view(b))
                        else {
                            unreachable!()
                        };
                        let x = x.get(i).unwrap().root(temp);
                        let y = y.get(i).unwrap().root(temp);
                        if equal(language, machine, temp, x, y, pos)? {
                            Ok(None)
                        } else {
                            compare(
                                language,
                                machine,
                                temp,
                                x,
                                y,
                                pos,
                                "while comparing two list elements",
                            )
                            .map(Some)
                        }
                    })?;
                    if let Some(cmp) = cmp {
                        return Ok(cmp);
                    }
                }
                Ok(nx < ny)
            }
            (x, y) => {
                let function = |v: &ValueView<'_, 'h>| {
                    matches!(
                        v,
                        ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_)
                    )
                };
                let same_type = matches!(
                    (&x, &y),
                    (ValueView::Bool(_), ValueView::Bool(_))
                        | (ValueView::Null, ValueView::Null)
                        | (ValueView::Attrs(_), ValueView::Attrs(_))
                ) || (function(&x) && function(&y));
                Err(language.legacy().error(
                    ErrorKind::Eval,
                    msg!(
                        "cannot compare ",
                        crate::managed_print::describe_type(language, machine, a),
                        " with ",
                        crate::managed_print::describe_type(language, machine, b),
                        if same_type {
                            "; values of that type are incomparable (values are "
                        } else {
                            "; values are "
                        },
                        print_for_error(language, machine, a),
                        " and ",
                        print_for_error(language, machine, b),
                        if same_type { ")" } else { "" }
                    ),
                ))
            }
        }
    });
    result.map_err(|e| e.with_trace(language.legacy().position(pos), ctx))
}

/// Permanent builtin metadata only; the higher-ranked signature prevents a
/// handler from retaining roots beyond the caller's bounded application scope.
/// Language::apply (or a direct caller) supplies that scope; handlers preserve
/// narrower per-element scopes but do not add a redundant outer frame.
pub(crate) type Handler = for<'a, 'r, 'h> fn(
    &Language<'a>,
    &mut Machine<'h>,
    &Roots<'r, 'h>,
    &str,
    &[CellRoot<'_, 'h>],
    SrcPos,
) -> Result<ValueRoot<'r, 'h>>;

pub(crate) fn resolve(name: &str) -> Handler {
    crate::managed_builtins_data::resolve(name)
        .or_else(|| strings::resolve(name))
        .or_else(|| crate::managed_builtins_paths::resolve(name))
        .or_else(|| crate::managed_builtins_derivation::resolve(name))
        .or_else(|| crate::managed_builtins_fetch::resolve(name))
        .unwrap_or(dispatch)
}

pub(crate) fn call<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    // Retain named dispatch for callers without permanent Primop metadata.
    if let Some(result) =
        crate::managed_builtins_data::call(language, machine, roots, name, args, pos)
    {
        return result;
    }
    if let Some(result) = strings::call(language, machine, roots, name, args, pos) {
        return result;
    }
    if let Some(result) =
        crate::managed_builtins_paths::call(language, machine, roots, name, args, pos)
    {
        return result;
    }
    if let Some(result) =
        crate::managed_builtins_derivation::call(language, machine, roots, name, args, pos)
    {
        return result;
    }
    if let Some(result) =
        crate::managed_builtins_fetch::call(language, machine, roots, name, args, pos)
    {
        return result;
    }
    dispatch(
        language,
        machine,
        roots,
        name.strip_prefix("__").unwrap_or(name),
        args,
        pos,
    )
}

fn dispatch<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let name = name.strip_prefix("__").unwrap_or(name);
    let ev = language.legacy();
    match name {
        "import" | "scopedImport" => {
            let arg = if name == "scopedImport" {
                args[1]
            } else {
                args[0]
            };
            let (accessor, path) =
                crate::managed_builtins_paths::realise_path(language, machine, roots, arg, pos)?;
            if path.ends_with(b".drv") {
                if let Some(sp) = ev.maybe_store_path(&path) {
                    if ev.config.store_dir.display(&sp).as_bytes() == path
                        && ev.store.borrow().contains(&sp)
                    {
                        return crate::managed_builtins_derivation::imported_drv_to_value(
                            language, machine, roots, pos, &path, &sp,
                        );
                    }
                }
            }
            let scope = if name == "scopedImport" {
                Some(force_attrs(
                    language,
                    machine,
                    roots,
                    args[0],
                    pos,
                    "while evaluating the first argument passed to builtins.scopedImport",
                )?)
            } else {
                None
            };
            crate::managed_import::file(
                language,
                machine,
                roots,
                &crate::value::PathVal {
                    accessor,
                    path: &path,
                },
                scope,
                false,
            )
        }
        "lazyTreeOutPath" | "lazyTreeNarHash" => {
            let id = force_int(
                language,
                machine,
                roots,
                args[0],
                pos,
                "while evaluating a lazy tree id",
            )?;
            let id = crate::lazy_tree::LazyTreeId(id as u32);
            if name == "lazyTreeOutPath" {
                let sp = ev.materialize(id)?;
                crate::managed_builtins_paths::store_path_string(language, machine, roots, &sp)
            } else {
                let hash = ev.lazy_tree_nar_hash(id)?;
                allocation(machine.heap.string(
                    roots,
                    hash.to_string(brix_store::HashFormat::Sri, true).as_bytes(),
                ))
            }
        }
        "genericClosure" => generic_closure(language, machine, roots, args[0]),
        "break" => language.force(machine, roots, args[0], pos),
        "seq" => {
            language.force_cell(machine, roots, args[0], pos)?;
            language.force(machine, roots, args[1], pos)
        }
        "deepSeq" => {
            deep_force(language, machine, roots, args[0], pos)?;
            language.force(machine, roots, args[1], pos)
        }
        "isNull" | "isInt" | "isFloat" | "isBool" | "isString" | "isPath" | "isFunction"
        | "typeOf" => {
            language.force_cell(machine, roots, args[0], pos)?;
            let ty = match machine.heap.read(args[0]) {
                ValueView::Int(_) => "int",
                ValueView::Float(_) => "float",
                ValueView::Bool(_) => "bool",
                ValueView::Null => "null",
                ValueView::String(_) => "string",
                ValueView::Path(_) => "path",
                ValueView::List(_) => "list",
                ValueView::Attrs(_) => "set",
                ValueView::Lambda { .. } | ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => {
                    "lambda"
                }
                _ => "thunk",
            };
            if name == "typeOf" {
                allocation(machine.heap.string(roots, ty.as_bytes()))
            } else {
                Ok(roots.bool(
                    ty == match name {
                        "isNull" => "null",
                        "isInt" => "int",
                        "isFloat" => "float",
                        "isBool" => "bool",
                        "isString" => "string",
                        "isPath" => "path",
                        _ => "lambda",
                    },
                ))
            }
        }
        "getEnv" => {
            force_string_no_ctx(
                language,
                machine,
                roots,
                args[0],
                pos,
                "while evaluating the first argument passed to builtins.getEnv",
            )?;
            allocation(machine.heap.string(roots, b""))
        }
        "abort" | "throw" => {
            let v = coerce_with_context(
                language,
                machine,
                roots,
                args[0],
                pos,
                crate::Coercion::COPY,
                if name == "abort" {
                    "while evaluating the error message passed to builtins.abort"
                } else {
                    "while evaluating the error message passed to builtin.throw"
                },
            )?;
            let v = strings::materialize(language, machine, roots, v)?;
            let bytes = strings::bytes(machine, v);
            Err(if name == "abort" {
                ev.error(
                    ErrorKind::Abort,
                    msg!(
                        "evaluation aborted with the following error message: '",
                        bytes,
                        "'"
                    ),
                )
            } else {
                ev.error(ErrorKind::Thrown, bytes)
            })
        }
        "tryEval" => {
            let (success, value) = match language.force(machine, roots, args[0], pos) {
                Ok(_) => (true, roots.copy(args[0])),
                Err(e) if e.is_catchable() => (
                    false,
                    allocation(machine.heap.cell(roots, roots.bool(false)))?,
                ),
                Err(e) => return Err(e),
            };
            let success = allocation(machine.heap.cell(roots, roots.bool(success)))?;
            let mut attrs = vec![
                crate::managed_heap::RootedAttr {
                    name: sym::VALUE,
                    pos: SrcPos::NONE,
                    value,
                },
                crate::managed_heap::RootedAttr {
                    name: ev.sym("success"),
                    pos: SrcPos::NONE,
                    value: success,
                },
            ];
            crate::managed_bindings::sort_dedup(&mut attrs);
            let set = allocation(crate::managed_bindings::from_sorted(
                &mut machine.heap,
                roots,
                &attrs,
                SrcPos::NONE,
            ))?;
            Ok(machine.heap.bindings_value(roots, set))
        }
        "addErrorContext" => match language.force(machine, roots, args[1], pos) {
            Ok(v) => Ok(v),
            #[cfg(brix_parallel)]
            Err(e) if e.kind == ErrorKind::Deferred => Err(e),
            Err(e) if matches!(e.kind, ErrorKind::OutOfMemory | ErrorKind::Foreign) => Err(e),
            Err(e) => {
                let v = coerce(language, machine, roots, args[0], pos, false, false)?;
                let v = strings::materialize(language, machine, roots, v)?;
                e.add_trace_always(strings::bytes(machine, v).to_vec());
                Err(e)
            }
        },
        "trace" | "traceVerbose" | "warn" => {
            if name == "traceVerbose" && !ev.config.trace_verbose {
                return language.force(machine, roots, args[1], pos);
            }
            let v = if name == "warn" {
                force_string(
                    language,
                    machine,
                    roots,
                    args[0],
                    pos,
                    "while evaluating the first argument; the message passed to builtins.warn",
                )?
            } else {
                language.force(machine, roots, args[0], pos)?
            };
            let bytes = if matches!(machine.heap.view(v), ValueView::String(_)) {
                let v = strings::materialize(language, machine, roots, v)?;
                strings::bytes(machine, v).to_vec()
            } else {
                crate::managed_print::print_value(
                    language,
                    machine,
                    roots,
                    args[0],
                    &crate::PrintOptions::default(),
                )?
                .to_vec()
            };
            if name == "warn" {
                ev.host.log(&crate::terminal::indent_message(
                    "evaluation warning",
                    &bytes,
                ));
            } else {
                ev.host.trace(&bytes);
            }
            language.force(machine, roots, args[1], pos)
        }
        "functionArgs" => {
            let v = language.force(machine, roots, args[0], pos)?;
            let mut attrs = Vec::new();
            match machine.heap.view(v) {
                ValueView::PrimOp(_) | ValueView::PrimOpApp(_) => {}
                ValueView::Lambda { node, .. } => {
                    let (code, lam) = ev.lambda_def(node);
                    if let Some(formals) = &lam.formals {
                        for f in formals.formals {
                            let value = allocation(
                                machine.heap.cell(roots, roots.bool(f.default.is_some())),
                            )?;
                            attrs.push(crate::managed_heap::RootedAttr {
                                name: f.name,
                                pos: ev.src_pos(code, f.pos),
                                value,
                            });
                        }
                    }
                }
                _ => {
                    return Err(ev.error_at(
                        ErrorKind::TypeError,
                        "'functionArgs' requires a function",
                        pos,
                    ));
                }
            }
            crate::managed_bindings::sort_dedup(&mut attrs);
            let set = allocation(crate::managed_bindings::from_sorted(
                &mut machine.heap,
                roots,
                &attrs,
                SrcPos::NONE,
            ))?;
            Ok(machine.heap.bindings_value(roots, set))
        }
        _ => Err(ev.error_at(
            ErrorKind::Eval,
            format!("managed builtin '{name}' has not been migrated"),
            pos,
        )),
    }
}

/// Operation-local weak identities are updated during copying collection.
pub(crate) fn deep_force<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<()> {
    fn walk<'r, 'h>(
        language: &Language<'_>,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
        visited: &crate::managed_heap::Visited<'h>,
    ) -> Result<()> {
        if !machine.heap.mark_visited(visited, cell) {
            return Ok(());
        }
        let _guard = language.legacy().enter(pos)?;
        stacker::maybe_grow(
            crate::stack::STACK_RED_ZONE,
            crate::stack::STACK_SEGMENT,
            || {
                roots.with_roots(|temp| {
                    let value = language.force(machine, temp, cell, pos)?;
                    match machine.heap.view(value) {
                        ValueView::List(l) => {
                            let len = l.len();
                            for i in 0..len {
                                temp.with_roots(|inner| {
                                    let ValueView::List(l) = machine.heap.view(value) else {
                                        unreachable!()
                                    };
                                    let cell = l.get(i).unwrap().root(inner);
                                    walk(language, machine, inner, cell, pos, visited).map_err(
                                        |e| {
                                            e.with_trace(
                                                None,
                                                format!(
                                                    "while evaluating list element at index {i}"
                                                ),
                                            )
                                        },
                                    )
                                })?;
                            }
                        }
                        ValueView::Attrs(b) => {
                            let set = b.root(temp);
                            let names: Vec<_> = b.iter().map(|a| a.name).collect();
                            for name in names {
                                temp.with_roots(|inner| {
                                    let attr = machine
                                        .heap
                                        .bindings_view(set)
                                        .get(name)
                                        .unwrap()
                                        .root(inner);
                                    walk(language, machine, inner, attr.value, attr.pos, visited)
                                        .map_err(|e| {
                                            e.with_trace(
                                                language.legacy().position(attr.pos),
                                                msg!(
                                                    "while evaluating the attribute '",
                                                    language.legacy().symbol_ident(name),
                                                    "'"
                                                ),
                                            )
                                        })
                                })?;
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                })
            },
        )
    }
    let visited = machine.heap.visited();
    walk(language, machine, roots, cell, pos, &visited)
}

fn generic_closure<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    out: &Roots<'r, 'h>,
    argument: CellRoot<'_, 'h>,
) -> Result<ValueRoot<'r, 'h>> {
    use crate::managed_heap::{CellKind, Lease};
    let pos = SrcPos::NONE;
    out.with_roots(|roots| {
        let attrs=force_attrs(language,machine,roots,argument,pos,"while evaluating the first argument passed to builtins.genericClosure")?;
        let required=|machine:&Machine<'h>,name,ctx:&str| {
            machine.heap.bindings_view(attrs).get(name).map(|a|a.value.root(roots)).ok_or_else(||language.legacy().error(ErrorKind::TypeError,msg!("attribute '",language.legacy().symbol_ident(name),"' missing")).with_trace(None,ctx))
        };
        let start=required(machine,sym::START_SET,"in the attrset passed as argument to builtins.genericClosure")?;
        let start=force_list(language,machine,roots,start,pos,"while evaluating the 'startSet' attribute passed as argument to builtins.genericClosure")?;
        let ValueView::List(list)=machine.heap.view(start) else {unreachable!()};
        if list.is_empty() {return Ok(out.copy(start));}
        let mut work:std::collections::VecDeque<Lease<'h,CellKind>>=(0..list.len()).map(|i|machine.heap.persist(list.get(i).unwrap().root(roots))).collect();
        let op=required(machine,sym::OPERATOR,"in the attrset passed as argument to builtins.genericClosure")?;
        let op=force_function(language,machine,roots,op,pos,"while evaluating the 'operator' attribute passed as argument to builtins.genericClosure")?;
        let mut result:Vec<Lease<'h,CellKind>>=Vec::new();
        let mut keys:Vec<(Lease<'h,CellKind>,Lease<'h,CellKind>)>=Vec::new();
        while let Some(elem)=work.pop_front() {
            roots.with_roots(|temp| {
                let cell=temp.copy(elem.root());
                let set=force_attrs(language,machine,temp,cell,pos,"").inspect_err(|err|err.add_trace(None,msg!("in genericClosure element ",print_for_error(language,machine,machine.heap.value(temp,cell)))))?;
                let key=machine.heap.bindings_view(set).get(sym::KEY).map(|a|a.value.root(temp)).ok_or_else(||language.legacy().error(ErrorKind::TypeError,"attribute 'key' missing")).inspect_err(|err|err.add_trace(None,msg!("in genericClosure element ",print_for_error(language,machine,machine.heap.value(temp,cell)))))?;
                language.force(machine,temp,key,pos)?;
                let mut lo=0;let mut hi=keys.len();
                while lo<hi {
                    let mid=(lo+hi)/2;
                    let (existing,other)=&keys[mid];
                    let comparison=(|| {
                        if compare(language,machine,temp,key,existing.root(),pos,"")? {Ok(std::cmp::Ordering::Less)}
                        else if compare(language,machine,temp,existing.root(),key,pos,"")? {Ok(std::cmp::Ordering::Greater)}
                        else {Ok(std::cmp::Ordering::Equal)}
                    })().inspect_err(|err: &crate::Error| {
                        err.add_trace(None,msg!("with element ",print_for_error(language,machine,machine.heap.value(temp,other.root()))));
                        err.add_trace(None,msg!("while comparing element ",print_for_error(language,machine,machine.heap.value(temp,cell))));
                    })?;
                    match comparison {std::cmp::Ordering::Less=>hi=mid,std::cmp::Ordering::Greater=>lo=mid+1,std::cmp::Ordering::Equal=>return Ok(())}
                }
                keys.insert(lo,(machine.heap.persist(key),elem.clone()));
                result.push(elem.clone());
                let next=(|| {
                    let next=language.call_value(machine,temp,op,&[cell],pos)?;
                    let next=allocation(machine.heap.cell(temp,next))?;
                    let next=force_list(language,machine,temp,next,pos,"while evaluating the return value of the `operator` passed to builtins.genericClosure")?;
                    let ValueView::List(list)=machine.heap.view(next) else {unreachable!()};let len=list.len();
                    for i in 0..len {temp.with_roots(|inner| {
                        let ValueView::List(list)=machine.heap.view(next) else {unreachable!()};
                        let item=list.get(i).unwrap().root(inner);
                        language.force(machine,inner,item,pos)?;
                        work.push_back(machine.heap.persist(item));Ok::<_,crate::Error>(())
                    })?;}
                    Ok::<_,crate::Error>(())
                })();
                next.inspect_err(|err|err.add_trace(None,msg!("while calling operator on genericClosure element ",print_for_error(language,machine,machine.heap.value(temp,cell)))))
            })?;
        }
        let cells:Vec<_>=result.iter().map(|v|v.root()).collect();
        allocation(machine.heap.list(out,&cells))
    })
}

/// Reproduce the parallel evaluator's observable prewalk order sequentially.
/// Its multi-item worker errors are memoized and rethrown by serialization.
pub(crate) fn deep_force_quiet<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<()> {
    type Work<'h> = (
        crate::managed_heap::Lease<'h, crate::managed_heap::CellKind>,
        SrcPos,
        bool,
    );
    fn walk<'h>(
        m: &mut Machine<'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
        root: bool,
        depth: usize,
        work: &mut Vec<Work<'h>>,
    ) {
        if depth > 1024
            || matches!(
                m.heap.read(cell),
                ValueView::String(_)
                    | ValueView::Path(_)
                    | ValueView::Null
                    | ValueView::Int(_)
                    | ValueView::Float(_)
                    | ValueView::Bool(_)
                    | ValueView::Failed(_)
            )
        {
            return;
        }
        if !m.heap.mark_seen(cell) && !root {
            return;
        }
        m.with_roots(|m, r| match m.heap.read(cell) {
            ValueView::Thunk { .. } | ValueView::App(_) | ValueView::Blackhole => {
                work.push((m.heap.persist(cell), pos, true))
            }
            ValueView::Attrs(b) => {
                if b.get(sym::TO_STRING).is_some() {
                    return;
                }
                if let Some(a) = b.get(sym::OUT_PATH) {
                    let a = a.root(r);
                    if matches!(
                        m.heap.read(a.value),
                        ValueView::Thunk { .. }
                            | ValueView::App(_)
                            | ValueView::Blackhole
                            | ValueView::Failed(_)
                    ) {
                        work.push((m.heap.persist(a.value), a.pos, false));
                    }
                } else {
                    let set = b.root(r);
                    let names: Vec<_> = b.iter().map(|a| a.name).collect();
                    for name in names {
                        r.with_roots(|t| {
                            let a = m.heap.bindings_view(set).get(name).unwrap().root(t);
                            walk(m, a.value, a.pos, false, depth + 1, work);
                        });
                    }
                }
            }
            ValueView::List(l) => {
                let value = l.root(r);
                let n = l.len();
                for i in 0..n {
                    r.with_roots(|t| {
                        let ValueView::List(l) = m.heap.view(value) else {
                            unreachable!()
                        };
                        let child = l.get(i).unwrap().root(t);
                        walk(m, child, pos, false, depth + 1, work);
                    });
                }
            }
            _ => {}
        });
    }
    fn run<'r, 'h>(
        l: &Language<'_>,
        m: &mut Machine<'h>,
        r: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
        swallow: bool,
    ) -> Result<()> {
        l.legacy().with_stack(|| {
            r.with_roots(|t| {
                if let Err(e) = l.force_cell(m, t, cell, pos) {
                    return if swallow { Ok(()) } else { Err(e) };
                }
                let mut work = Vec::new();
                walk(m, cell, pos, true, 0, &mut work);
                let many = work.len() > 1;
                for (cell, at, deep) in work {
                    let at = if many { SrcPos::NONE } else { at };
                    let result = if deep {
                        run(l, m, t, cell.root(), at, swallow || many)
                    } else {
                        l.force_cell(m, t, cell.root(), at)
                    };
                    if !swallow && !many {
                        result?;
                    }
                }
                Ok(())
            })
        })
    }
    run(language, machine, roots, cell, pos, false)
}
