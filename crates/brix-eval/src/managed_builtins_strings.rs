//! String and string-context builtins over rooted values.
//!
//! Owned byte snapshots cross calls that can collect. They contain no runtime
//! graph: managed values remain in scoped roots and registered vectors.

use crate::runtime::rc::Rc;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use brix_store::{Hash, HashFormat, StorePath};

use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::lazy_tree::{LazyTreeId, HASH_LEN};
use crate::managed_bindings;
use crate::managed_builtins::coerce_with_context;
use crate::managed_builtins::{
    force_attrs, force_bool, force_int, force_string, force_string_no_ctx as no_context,
    force_string_no_ctx_value as no_context_value,
};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{
    BindingsRoot, CellKind, CellRoot, ContextInput, ContextRoot, ContextView, RootedAttr, Roots,
    ValueRoot, ValueView,
};
use crate::managed_language::Language;
use crate::symbol::{sym, Symbol};
use crate::toml_datetime::normalize_toml_datetime;
use crate::value::SrcPos;
use crate::xml_writer::XmlWriter;
use crate::{Error, Result};

/// Same ordering as `ContextElem`; owned bytes never point into the moving heap.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OwnedContext {
    LazyHash { offset: u32, tree: u32 },
    LazyPath(u32),
    Opaque(Vec<u8>),
    DrvDeep(Vec<u8>),
    Built { drv: Vec<u8>, output: Vec<u8> },
    Path(Vec<u8>),
}

impl OwnedContext {
    pub(crate) fn is_real(&self) -> bool {
        matches!(
            self,
            Self::Opaque(_) | Self::DrvDeep(_) | Self::Built { .. }
        )
    }
}

/// Whether realising this context can build a derivation (import from
/// derivation). A build is an ordered effect: a background spark stops
/// before one. Source and store paths need none.
pub(crate) fn needs_build(context: &[OwnedContext]) -> bool {
    context
        .iter()
        .any(|elem| matches!(elem, OwnedContext::DrvDeep(_) | OwnedContext::Built { .. }))
}

/// Borrow bytes for immediate consumption. The Machine borrow excludes all
/// managed allocation and mutation; callers retain the root between borrows.
pub(crate) fn bytes<'v, 'h>(machine: &'v Machine<'h>, value: ValueRoot<'_, 'h>) -> &'v [u8] {
    let ValueView::String(string) = machine.heap.view(value) else {
        panic!("string value required")
    };
    string.bytes()
}

pub(crate) fn snapshot<'h>(
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
) -> (Vec<u8>, Vec<OwnedContext>) {
    let ValueView::String(value) = machine.heap.view(value) else {
        panic!("string value required")
    };
    let context = value.contexts().map(own_context).collect();
    (value.bytes().to_vec(), context)
}

fn own_context(context: ContextView<'_>) -> OwnedContext {
    match context {
        ContextView::LazyHash { offset, tree } => OwnedContext::LazyHash { offset, tree },
        ContextView::LazyPath(tree) => OwnedContext::LazyPath(tree),
        ContextView::Opaque(path) => OwnedContext::Opaque(path.to_vec()),
        ContextView::DrvDeep(path) => OwnedContext::DrvDeep(path.to_vec()),
        ContextView::Built { drv, output } => OwnedContext::Built {
            drv: drv.to_vec(),
            output: output.to_vec(),
        },
        ContextView::Path(path) => OwnedContext::Path(path.to_vec()),
    }
}

/// Append context to an owned consumer without an intermediate vector. Unlike
/// concatenation, derivation/JSON/XML consumers keep context offsets unchanged.
pub(crate) fn extend_context<'h>(
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
    output: &mut Vec<OwnedContext>,
) {
    let ValueView::String(value) = machine.heap.view(value) else {
        panic!("string value required")
    };
    output.extend(value.contexts().map(own_context));
}

/// Append directly from a bounded heap borrow. Only the resulting buffers are
/// owned: no intermediate byte vector or context vector is needed per part.
/// Lazy hash positions are shifted to their positions in the combined string.
pub(crate) fn append_snapshot<'h>(
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
    output: &mut Vec<u8>,
    context: &mut Vec<OwnedContext>,
) -> Result<()> {
    append_context_snapshot(machine, value, output.len(), context)?;
    output.extend_from_slice(bytes(machine, value));
    Ok(())
}

/// Copy context and validate shifted hash offsets as each part is evaluated.
/// Keeping these checks here preserves error ordering before later parts run.
pub(crate) fn append_context_snapshot<'h>(
    machine: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
    prefix: usize,
    context: &mut Vec<OwnedContext>,
) -> Result<usize> {
    let ValueView::String(value) = machine.heap.view(value) else {
        panic!("string value required")
    };
    for elem in value.contexts() {
        let mut elem = own_context(elem);
        if let OwnedContext::LazyHash { offset, .. } = &mut elem {
            let prefix = u32::try_from(prefix).map_err(|_| {
                crate::error::EvalError::new(ErrorKind::Eval, "string is too large")
            })?;
            *offset = offset.checked_add(prefix).ok_or_else(|| {
                crate::error::EvalError::new(ErrorKind::Eval, "string is too large")
            })?;
        }
        context.push(elem);
    }
    prefix
        .checked_add(value.bytes().len())
        .ok_or_else(|| crate::error::EvalError::new(ErrorKind::Eval, "string is too large"))
}

pub(crate) type ContextSegments<'r, 'h> = smallvec::SmallVec<[(ValueRoot<'r, 'h>, usize); 8]>;

/// Validate immediately, before a later expression can fail, but retain rooted
/// context sources rather than copying their managed bytes into owned buffers.
pub(crate) fn append_context_segment<'r, 'h>(
    machine: &Machine<'h>,
    value: ValueRoot<'r, 'h>,
    prefix: usize,
    segments: &mut ContextSegments<'r, 'h>,
) -> Result<usize> {
    let ValueView::String(string) = machine.heap.view(value) else {
        panic!("string value required")
    };
    let mut has_context = false;
    for context in string.contexts() {
        has_context = true;
        if let ContextView::LazyHash { offset, .. } = context {
            u32::try_from(prefix)
                .ok()
                .and_then(|prefix| offset.checked_add(prefix))
                .ok_or_else(|| {
                    crate::error::EvalError::new(ErrorKind::Eval, "string is too large")
                })?;
        }
    }
    let length = prefix
        .checked_add(string.bytes().len())
        .ok_or_else(|| crate::error::EvalError::new(ErrorKind::Eval, "string is too large"))?;
    if has_context {
        segments.push((value, prefix));
    }
    Ok(length)
}

fn snapshot_context<'h>(machine: &Machine<'h>, value: ValueRoot<'_, 'h>) -> Vec<OwnedContext> {
    let ValueView::String(value) = machine.heap.view(value) else {
        panic!("string value required")
    };
    value.contexts().map(own_context).collect()
}

fn context_root<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    context: &[OwnedContext],
) -> Result<ContextRoot<'r, 'h>> {
    if context.is_empty() {
        return Ok(roots.empty_context());
    }
    let mut sorted: smallvec::SmallVec<[&OwnedContext; 4]> = context.iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut inputs: smallvec::SmallVec<[ContextInput<'_, '_>; 4]> =
        smallvec::SmallVec::with_capacity(sorted.len());
    for elem in sorted {
        inputs.push(match elem {
            OwnedContext::LazyHash { offset, tree } => ContextInput::LazyHash {
                offset: *offset,
                tree: *tree,
            },
            OwnedContext::LazyPath(tree) => ContextInput::LazyPath(*tree),
            OwnedContext::Opaque(path) => {
                ContextInput::Opaque(allocation(machine.heap.bytes(roots, path))?)
            }
            OwnedContext::DrvDeep(path) => {
                ContextInput::DrvDeep(allocation(machine.heap.bytes(roots, path))?)
            }
            OwnedContext::Path(path) => {
                ContextInput::Path(allocation(machine.heap.bytes(roots, path))?)
            }
            OwnedContext::Built { drv, output } => {
                let drv = allocation(machine.heap.bytes(roots, drv))?;
                let output = allocation(machine.heap.bytes(roots, output))?;
                ContextInput::Built { drv, output }
            }
        });
    }
    allocation(machine.heap.contexts(roots, &inputs))
}

pub(crate) fn string<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    bytes: &[u8],
    context: &[OwnedContext],
) -> Result<ValueRoot<'r, 'h>> {
    if context.is_empty() {
        return allocation(machine.heap.string(roots, bytes));
    }
    machine.with_roots(|machine, temporary| {
        let context = context_root(machine, temporary, context)?;
        let value = allocation(machine.heap.string_with_context(temporary, bytes, context))?;
        Ok(roots.copy(value))
    })
}

/// Rooted inputs allow one collecting boundary for bytes, context and header.
pub(crate) fn concat_parts<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    parts: &[ValueRoot<'_, 'h>],
    segments: &[(ValueRoot<'_, 'h>, usize)],
) -> Result<ValueRoot<'r, 'h>> {
    allocation(machine.heap.concat_strings(roots, parts, segments))
}

fn shared_slice<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    source: ValueRoot<'_, 'h>,
    range: std::ops::Range<usize>,
    context: &[OwnedContext],
) -> Result<ValueRoot<'r, 'h>> {
    machine.with_roots(|machine, temporary| {
        let context = context_root(machine, temporary, context)?;
        let value = allocation(
            machine
                .heap
                .substring_with_context(temporary, source, range, context),
        )?;
        Ok(roots.copy(value))
    })
}

fn recontextualize<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    source: ValueRoot<'_, 'h>,
    context: &[OwnedContext],
) -> Result<ValueRoot<'r, 'h>> {
    let ValueView::String(value) = machine.heap.view(source) else {
        panic!("string value required")
    };
    let len = value.bytes().len();
    shared_slice(machine, roots, source, 0..len, context)
}

/// Compare known bytes and lazy hash identities before asking the host to hash
/// a tree. Rooted strings are only borrowed here; no collection can occur.
pub(crate) fn known_comparison<'h>(
    machine: &Machine<'h>,
    left: ValueRoot<'_, 'h>,
    right: ValueRoot<'_, 'h>,
    less: bool,
) -> Option<bool> {
    let (ValueView::String(left), ValueView::String(right)) =
        (machine.heap.view(left), machine.heap.view(right))
    else {
        unreachable!("string comparison requires two strings")
    };
    known_view_comparison(left, right, less)
}

/// Borrowed variant for already rooted cells. The heap borrow prevents GC, so
/// inspecting known strings needs no additional value snapshots.
#[inline]
pub(crate) fn known_view_comparison<'h>(
    left: crate::managed_heap::StringView<'_, 'h>,
    right: crate::managed_heap::StringView<'_, 'h>,
    less: bool,
) -> Option<bool> {
    let x = left.bytes();
    let y = right.bytes();
    if !less && x.len() != y.len() {
        return Some(false);
    }
    if !left.has_lazy_hash() && !right.has_lazy_hash() {
        return Some(if less { x < y } else { x == y });
    }
    lazy_view_comparison(left, right, less)
}

#[inline(never)]
fn lazy_view_comparison<'h>(
    left: crate::managed_heap::StringView<'_, 'h>,
    right: crate::managed_heap::StringView<'_, 'h>,
    less: bool,
) -> Option<bool> {
    use crate::lazy_tree::{certainly_differ, compare_lazy, LazyHash};
    use smallvec::SmallVec;
    let hashes = |value: crate::managed_heap::StringView<'_, 'h>| {
        value
            .contexts()
            .filter_map(|context| match context {
                ContextView::LazyHash { offset, tree } => Some(LazyHash {
                    offset: offset as usize,
                    tree,
                }),
                _ => None,
            })
            .collect::<SmallVec<[LazyHash; 1]>>()
    };
    let x = left.bytes();
    let y = right.bytes();
    let hx = hashes(left);
    let hy = hashes(right);
    if less {
        compare_lazy(x, &hx, y, &hy).map(|order| order.is_lt())
    } else if hx == hy {
        Some(x == y)
    } else if certainly_differ(x, &hx, y, &hy) {
        Some(false)
    } else {
        None
    }
}

/// Resolves text hashes, preserving position-independent hidden path context
/// as `str_parts` does. Merely carrying a LazyPath must not hash its tree.
pub(crate) fn materialize<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    value: ValueRoot<'_, 'h>,
) -> Result<ValueRoot<'r, 'h>> {
    let has_hash = match machine.heap.view(value) {
        ValueView::String(value) => value
            .contexts()
            .any(|elem| matches!(elem, ContextView::LazyHash { .. })),
        _ => panic!("materialize requires a string"),
    };
    if !has_hash {
        return Ok(roots.copy(value));
    }
    let (mut bytes, mut context) = snapshot(machine, value);
    for elem in &context {
        if let OwnedContext::LazyHash { offset, tree } = elem {
            let path = machine
                .heap
                .blocking(|| language.legacy().materialize(LazyTreeId(*tree)))?;
            let offset = *offset as usize;
            bytes[offset..offset + HASH_LEN].copy_from_slice(path.hash_part().as_bytes());
        }
    }
    context.retain(|elem| !matches!(elem, OwnedContext::LazyHash { .. }));
    string(machine, roots, &bytes, &context)
}

pub(crate) fn resolve_context_parked(
    language: &Language<'_>,
    machine: &mut Machine<'_>,
    context: &mut Vec<OwnedContext>,
) -> Result<()> {
    // Ordinary strings need no host call, so avoid the rendezvous mutexes.
    #[cfg(brix_parallel)]
    if context
        .iter()
        .any(|elem| matches!(elem, OwnedContext::LazyPath(_)))
    {
        return machine.heap.blocking(|| resolve_context(language, context));
    }
    #[cfg(not(brix_parallel))]
    let _ = machine;
    resolve_context(language, context)
}

pub(crate) fn resolve_context(
    language: &Language<'_>,
    context: &mut Vec<OwnedContext>,
) -> Result<()> {
    for elem in context.iter_mut() {
        if let OwnedContext::LazyPath(tree) = elem {
            *elem = OwnedContext::Path(
                language
                    .legacy()
                    .materialize(LazyTreeId(*tree))?
                    .as_str()
                    .as_bytes()
                    .to_vec(),
            );
        }
    }
    context.sort_unstable();
    context.dedup();
    Ok(())
}

fn force_list<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    context: &str,
) -> Result<Vec<CellRoot<'r, 'h>>> {
    let value = crate::managed_builtins::force_list(language, machine, roots, cell, pos, context)?;
    let ValueView::List(list) = machine.heap.view(value) else {
        unreachable!()
    };
    Ok((0..list.len())
        .map(|index| list.get(index).unwrap().root(roots))
        .collect())
}

fn string_parts<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    context: &str,
) -> Result<(Vec<u8>, Vec<OwnedContext>)> {
    let value = force_string(language, machine, roots, cell, pos, context)?;
    let value = materialize(language, machine, roots, value)?;
    Ok(snapshot(machine, value))
}

fn attrs_value<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    mut attrs: Vec<RootedAttr<'_, 'h>>,
) -> Result<ValueRoot<'r, 'h>> {
    managed_bindings::sort_dedup(&mut attrs);
    let set = allocation(managed_bindings::from_sorted(
        &mut machine.heap,
        roots,
        &attrs,
        SrcPos::NONE,
    ))?;
    Ok(machine.heap.bindings_value(roots, set))
}

fn attr<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: Symbol,
    value: ValueRoot<'_, 'h>,
) -> Result<RootedAttr<'r, 'h>> {
    Ok(RootedAttr {
        name,
        pos: SrcPos::NONE,
        value: allocation(machine.heap.cell(roots, value))?,
    })
}

pub(crate) fn call<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Option<Result<ValueRoot<'r, 'h>>> {
    let handler = resolve(name)?;
    Some(handler(
        language,
        machine,
        roots,
        name.strip_prefix("__").unwrap_or(name),
        args,
        pos,
    ))
}

pub(crate) fn resolve(name: &str) -> Option<super::managed_builtins::Handler> {
    let name = name.strip_prefix("__").unwrap_or(name);
    let recognized = matches!(
        name,
        "toString"
            | "substring"
            | "stringLength"
            | "hashString"
            | "convertHash"
            | "match"
            | "split"
            | "concatStringsSep"
            | "replaceStrings"
            | "parseDrvName"
            | "compareVersions"
            | "splitVersion"
            | "fromJSON"
            | "toJSON"
            | "toXML"
            | "fromTOML"
            | "unsafeDiscardStringContext"
            | "hasContext"
            | "unsafeDiscardOutputDependency"
            | "addDrvOutputDependencies"
            | "getContext"
            | "appendContext"
    );
    if !recognized {
        return None;
    }
    Some(dispatch)
}

fn dispatch<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    match name {
        "toString" => coerce_with_context(
            language,
            machine,
            roots,
            args[0],
            pos,
            Coercion::MORE,
            "while evaluating the first argument passed to builtins.toString",
        ),
        "substring" => substring(language, machine, roots, args, pos),
        "stringLength" => {
            let value = coerce_with_context(
                language,
                machine,
                roots,
                args[0],
                pos,
                Coercion::COPY,
                "while evaluating the argument passed to builtins.stringLength",
            )?;
            let ValueView::String(string) = machine.heap.view(value) else {
                unreachable!()
            };
            Ok(roots.int(string.bytes().len() as i64))
        }
        "hashString" => {
            let algo = no_context_value(
                language,
                machine,
                roots,
                args[0],
                pos,
                "while evaluating the first argument passed to builtins.hashString",
            )?;
            let algo = crate::builtins::strings::parse_hash_algo(
                language.legacy(),
                bytes(machine, algo),
                pos,
            )?;
            let value = force_string(
                language,
                machine,
                roots,
                args[1],
                pos,
                "while evaluating the second argument passed to builtins.hashString",
            )?;
            let value = materialize(language, machine, roots, value)?;
            let hash = Hash::of(algo, bytes(machine, value)).to_string(HashFormat::Base16, false);
            allocation(machine.heap.string(roots, hash.as_bytes()))
        }
        "convertHash" => convert_hash(language, machine, roots, args[0], pos),
        "match" | "split" => regex_builtin(language, machine, roots, name, args, pos),
        "concatStringsSep" => concat(language, machine, roots, args, pos),
        "replaceStrings" => replace(language, machine, roots, args, pos),
        "parseDrvName" => {
            let bytes = no_context(
                language,
                machine,
                roots,
                args[0],
                pos,
                "while evaluating the first argument passed to builtins.parseDrvName",
            )?;
            let (name, version) = crate::builtins::strings::parse_drv_name(&bytes);
            let name_value = allocation(machine.heap.string(roots, name))?;
            let version_value = allocation(machine.heap.string(roots, version))?;
            let fields = vec![
                attr(machine, roots, sym::NAME, name_value)?,
                attr(
                    machine,
                    roots,
                    language.legacy().sym("version"),
                    version_value,
                )?,
            ];
            attrs_value(machine, roots, fields)
        }
        "compareVersions" => {
            let left = no_context_value(
                language,
                machine,
                roots,
                args[0],
                pos,
                "while evaluating the first argument passed to builtins.compareVersions",
            )?;
            let right = no_context_value(
                language,
                machine,
                roots,
                args[1],
                pos,
                "while evaluating the second argument passed to builtins.compareVersions",
            )?;
            Ok(roots.int(
                match crate::builtins::strings::compare_versions(
                    bytes(machine, left),
                    bytes(machine, right),
                ) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                },
            ))
        }
        "splitVersion" => split_version(language, machine, roots, args[0], pos),
        "unsafeDiscardStringContext"
        | "hasContext"
        | "unsafeDiscardOutputDependency"
        | "addDrvOutputDependencies"
        | "getContext"
        | "appendContext" => context_builtin(language, machine, roots, name, args, pos),
        "fromJSON" => from_json(language, machine, roots, args[0], pos),
        "toJSON" => to_json(language, machine, roots, args[0], pos),
        "fromTOML" => from_toml(language, machine, roots, args[0], pos),
        "toXML" => to_xml(language, machine, roots, args[0], pos),
        _ => unreachable!("dispatch follows recognition"),
    }
}

fn substring<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let start = force_int(
        language,
        machine,
        roots,
        args[0],
        pos,
        "while evaluating the first argument (the start offset) passed to builtins.substring",
    )?;
    if start < 0 {
        return Err(language.legacy().error_at(
            ErrorKind::Eval,
            "negative start position in 'substring'",
            pos,
        ));
    }
    let length = force_int(
        language,
        machine,
        roots,
        args[1],
        pos,
        "while evaluating the second argument (the substring length) passed to builtins.substring",
    )?;
    let mut input = coerce_with_context(
        language,
        machine,
        roots,
        args[2],
        pos,
        Coercion::COPY,
        "while evaluating the third argument (the string) passed to builtins.substring",
    )?;
    let ValueView::String(value) = machine.heap.view(input) else {
        unreachable!()
    };
    let len = value.bytes().len();
    let has_hash = value
        .contexts()
        .any(|elem| matches!(elem, ContextView::LazyHash { .. }));
    let start = (start as usize).min(len);
    let end = if length < 0 {
        len
    } else {
        start.saturating_add(length as usize).min(len)
    };
    if !has_hash {
        return allocation(machine.heap.substring(roots, input, start..end));
    }
    let mut context = snapshot_context(machine, input);
    let cut = length != 0
        && context.iter().any(|elem| match elem {
            OwnedContext::LazyHash { offset, .. } => {
                let a = *offset as usize;
                let b = a + HASH_LEN;
                a < end && b > start && !(a >= start && b <= end)
            }
            _ => false,
        });
    if cut {
        input = materialize(language, machine, roots, input)?;
        context = snapshot_context(machine, input);
        resolve_context_parked(language, machine, &mut context)?;
    } else {
        context.retain_mut(|elem| match elem {
            OwnedContext::LazyHash { offset, .. }
                if (*offset as usize) >= start && (*offset as usize) + HASH_LEN <= end =>
            {
                *offset -= start as u32;
                true
            }
            OwnedContext::LazyHash { .. } => false,
            _ => true,
        });
    }
    shared_slice(machine, roots, input, start..end, &context)
}

fn require<'r, 'h>(
    language: &Language<'_>,
    machine: &Machine<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    name: Symbol,
    context: &str,
) -> Result<RootedAttr<'r, 'h>> {
    let view = machine.heap.bindings_view(set);
    view.get(name).map(|attr| attr.root(roots)).ok_or_else(|| {
        language
            .legacy()
            .error(
                ErrorKind::TypeError,
                msg!(
                    "attribute '",
                    language.legacy().symbol_ident(name),
                    "' missing"
                ),
            )
            .with_trace(None, context)
    })
}

fn convert_hash<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let attrs = force_attrs(
        language,
        machine,
        roots,
        cell,
        pos,
        "while evaluating the first argument passed to builtins.convertHash",
    )?;
    let field = require(
        language,
        machine,
        roots,
        attrs,
        language.legacy().sym("hash"),
        "while locating the attribute 'hash'",
    )?;
    let hash = no_context_value(
        language,
        machine,
        roots,
        field.value,
        pos,
        "while evaluating the attribute 'hash'",
    )?;
    let algo = machine
        .heap
        .bindings_view(attrs)
        .get(language.legacy().sym("hashAlgo"))
        .map(|attr| attr.root(roots));
    let algo = match algo {
        None => None,
        Some(attr) => {
            let algorithm = no_context_value(
                language,
                machine,
                roots,
                attr.value,
                pos,
                "while evaluating the attribute 'hashAlgo'",
            )?;
            Some(crate::builtins::strings::parse_hash_algo(
                language.legacy(),
                bytes(machine, algorithm),
                pos,
            )?)
        }
    };
    let field = require(
        language,
        machine,
        roots,
        attrs,
        language.legacy().sym("toHashFormat"),
        "while locating the attribute 'toHashFormat'",
    )?;
    let format = no_context_value(
        language,
        machine,
        roots,
        field.value,
        pos,
        "while evaluating the attribute 'toHashFormat'",
    )?;
    let (format, warning) = HashFormat::parse(bytes(machine, format))
        .map_err(|error| crate::builtins::strings::hash_error(language.legacy(), &error))?;
    if let Some(warning) = warning {
        crate::runtime::ordered_effect()?;
        language.legacy().host.warn(warning.as_bytes());
    }
    let hash = Hash::parse_any(
        bytes(machine, hash),
        algo,
        crate::builtins::strings::xp_settings(language.legacy()),
    )
    .map_err(|error| crate::builtins::strings::hash_error(language.legacy(), &error))?;
    allocation(machine.heap.string(
        roots,
        hash.to_string(format, format == HashFormat::Sri).as_bytes(),
    ))
}

fn regex(language: &Language<'_>, bytes: &[u8], pos: SrcPos) -> Result<Rc<crate::regex::Regex>> {
    if let Some(regex) = language.legacy().regex_cache.borrow().get(bytes) {
        return Ok(regex.clone());
    }
    let regex = Rc::new(
        crate::regex::Regex::new(bytes)
            .inspect_err(|error| error.set_pos(language.legacy().position(pos)))?,
    );
    language
        .legacy()
        .regex_cache
        .borrow_mut()
        .insert(bytes.to_vec(), regex.clone());
    Ok(regex)
}

fn captures<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    captures: &regex::bytes::Captures<'_>,
) -> Result<ValueRoot<'r, 'h>> {
    let values = roots.vector::<CellKind>();
    for index in 1..captures.len() {
        machine.with_roots(|machine, temporary| {
            let value = match captures.get(index) {
                Some(value) => allocation(machine.heap.string(temporary, value.as_bytes()))?,
                None => temporary.null(),
            };
            values.push(allocation(machine.heap.cell(temporary, value))?);
            Ok::<(), Error>(())
        })?;
    }
    let cells: Vec<_> = (0..values.len())
        .map(|index| values.get(roots, index).unwrap())
        .collect();
    allocation(machine.heap.list(roots, &cells))
}

fn regex_builtin<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let pattern = no_context_value(
        language,
        machine,
        roots,
        args[0],
        pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    let regex = regex(language, bytes(machine, pattern), pos)?;
    // Compile the appropriate regexp before forcing the haystack.
    let compiled = if name == "match" {
        regex.whole()
    } else {
        regex.search()
    }
    .inspect_err(|error| error.set_pos(language.legacy().position(pos)))?;
    let (bytes, _) = string_parts(
        language,
        machine,
        roots,
        args[1],
        pos,
        &format!("while evaluating the second argument passed to builtins.{name}"),
    )?;
    if name == "match" {
        return match compiled.captures(&bytes) {
            Some(found) => captures(machine, roots, &found),
            None => Ok(roots.null()),
        };
    }
    let values = roots.vector::<CellKind>();
    let (mut last, mut position, mut previous_empty) = (0, 0, None);
    let mut any = false;
    while position <= bytes.len() {
        let Some(found) = compiled.captures_at(&bytes, position) else {
            break;
        };
        let whole = found.get(0).unwrap();
        if whole.is_empty() && previous_empty == Some(whole.start()) {
            if whole.start() >= bytes.len() {
                break;
            }
            position = whole.start() + 1;
            previous_empty = None;
            continue;
        }
        any = true;
        machine.with_roots(|machine, temporary| {
            let prefix = allocation(machine.heap.string(temporary, &bytes[last..whole.start()]))?;
            values.push(allocation(machine.heap.cell(temporary, prefix))?);
            let groups = captures(machine, temporary, &found)?;
            values.push(allocation(machine.heap.cell(temporary, groups))?);
            Ok::<(), Error>(())
        })?;
        last = whole.end();
        position = whole.end();
        previous_empty = whole.is_empty().then_some(whole.end());
    }
    if !any {
        return allocation(machine.heap.list(roots, &[args[1]]));
    }
    let tail = allocation(machine.heap.string(roots, &bytes[last..]))?;
    values.push(allocation(machine.heap.cell(roots, tail))?);
    let cells: Vec<_> = (0..values.len())
        .map(|index| values.get(roots, index).unwrap())
        .collect();
    allocation(machine.heap.list(roots, &cells))
}

fn concat<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let separator = force_string(language, machine, roots, args[0], pos, "while evaluating the first argument (the separator string) passed to builtins.concatStringsSep")?;
    let separator = materialize(language, machine, roots, separator)?;
    // Separator context remains attached even when the list is empty, as before.
    let mut segments = ContextSegments::new();
    append_context_segment(machine, separator, 0, &mut segments)?;
    let list = force_list(language, machine, roots, args[1], pos, "while evaluating the second argument (the list of strings to concat) passed to builtins.concatStringsSep")?;
    let mut parts: smallvec::SmallVec<[_; 8]> = smallvec::SmallVec::new();
    let mut length = 0usize;
    for (index, cell) in list.into_iter().enumerate() {
        if index != 0 {
            length = length
                .checked_add(bytes(machine, separator).len())
                .ok_or_else(|| {
                    crate::error::EvalError::new(ErrorKind::Eval, "string is too large")
                })?;
            parts.push(separator);
        }
        machine.with_roots(|machine, temporary| {
            let value = coerce_with_context(language, machine, temporary, cell, pos, Coercion::COPY, "while evaluating one element of the list of strings to concat passed to builtins.concatStringsSep")?;
            // Only each coerced result survives the loop's bounded scope.
            let value = roots.copy(value);
            length = append_context_segment(machine, value, length, &mut segments)?;
            parts.push(value);
            Ok::<(), Error>(())
        })?;
    }
    concat_parts(machine, roots, &parts, &segments)
}

fn replace<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let from = force_list(
        language,
        machine,
        roots,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.replaceStrings",
    )?;
    let to = force_list(
        language,
        machine,
        roots,
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.replaceStrings",
    )?;
    if from.len() != to.len() {
        return Err(language.legacy().error_at(
            ErrorKind::Eval,
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths",
            pos,
        ));
    }
    let mut patterns = Vec::with_capacity(from.len());
    for cell in from {
        patterns.push(machine.with_roots(|machine, temporary| {
            string_parts(
                language,
                machine,
                temporary,
                cell,
                pos,
                "while evaluating one of the strings to replace passed to builtins.replaceStrings",
            )
            .map(|(bytes, _)| bytes)
        })?);
    }
    let (input, mut context) = string_parts(
        language,
        machine,
        roots,
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.replaceStrings",
    )?;
    let mut cached: Vec<Option<Vec<u8>>> = vec![None; patterns.len()];
    let mut bytes = Vec::with_capacity(input.len());
    let mut position = 0;
    while position <= input.len() {
        let mut found = false;
        for (index, pattern) in patterns.iter().enumerate() {
            if !input[position..].starts_with(pattern) {
                continue;
            }
            found = true;
            if cached[index].is_none() {
                let (replacement, more) = machine.with_roots(|machine, temporary| string_parts(language, machine, temporary, to[index], pos, "while evaluating one of the replacement strings passed to builtins.replaceStrings"))?;
                context.extend(more);
                cached[index] = Some(replacement);
            }
            bytes.extend_from_slice(cached[index].as_ref().unwrap());
            if pattern.is_empty() {
                if position < input.len() {
                    bytes.push(input[position]);
                }
                position += 1;
            } else {
                position += pattern.len();
            }
            break;
        }
        if !found {
            if position < input.len() {
                bytes.push(input[position]);
            }
            position += 1;
        }
    }
    string(machine, roots, &bytes, &context)
}

fn split_version<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let bytes = no_context(
        language,
        machine,
        roots,
        cell,
        pos,
        "while evaluating the first argument passed to builtins.splitVersion",
    )?;
    let values = roots.vector::<CellKind>();
    let mut position = 0;
    while position < bytes.len() {
        while position < bytes.len() && matches!(bytes[position], b'.' | b'-') {
            position += 1;
        }
        let start = position;
        if position == bytes.len() {
            break;
        }
        let numeric = bytes[position].is_ascii_digit();
        while position < bytes.len()
            && !matches!(bytes[position], b'.' | b'-')
            && bytes[position].is_ascii_digit() == numeric
        {
            position += 1;
        }
        machine.with_roots(|machine, temporary| {
            let value = allocation(machine.heap.string(temporary, &bytes[start..position]))?;
            values.push(allocation(machine.heap.cell(temporary, value))?);
            Ok::<(), Error>(())
        })?;
    }
    let cells: Vec<_> = (0..values.len())
        .map(|index| values.get(roots, index).unwrap())
        .collect();
    allocation(machine.heap.list(roots, &cells))
}

fn context_builtin<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    if name == "hasContext" {
        let value = force_string(
            language,
            machine,
            roots,
            args[0],
            pos,
            "while evaluating the argument passed to builtins.hasContext",
        )?;
        let ValueView::String(value) = machine.heap.view(value) else {
            unreachable!()
        };
        return Ok(roots.bool(value.contexts().any(|elem| {
            matches!(
                elem,
                ContextView::Opaque(_) | ContextView::DrvDeep(_) | ContextView::Built { .. }
            )
        })));
    }
    if name == "getContext" {
        return get_context(language, machine, roots, args[0], pos);
    }
    if name == "appendContext" {
        return append_context(language, machine, roots, args, pos);
    }
    let value = coerce_with_context(
        language,
        machine,
        roots,
        args[0],
        pos,
        Coercion::COPY,
        &format!("while evaluating the argument passed to builtins.{name}"),
    )?;
    if name == "unsafeDiscardStringContext" {
        let mut context = snapshot_context(machine, value);
        context.retain(|elem| {
            matches!(
                elem,
                OwnedContext::LazyHash { .. } | OwnedContext::LazyPath(_) | OwnedContext::Path(_)
            )
        });
        return recontextualize(machine, roots, value, &context);
    }
    let value = materialize(language, machine, roots, value)?;
    let mut context = snapshot_context(machine, value);
    if name == "unsafeDiscardOutputDependency" {
        for elem in &mut context {
            if let OwnedContext::DrvDeep(path) = elem {
                *elem = OwnedContext::Opaque(std::mem::take(path));
            }
        }
        return recontextualize(machine, roots, value, &context);
    }
    if context.len() != 1 {
        let ValueView::String(value) = machine.heap.view(value) else {
            unreachable!()
        };
        return Err(language.legacy().error_at(
            ErrorKind::Eval,
            msg!(
                "context of string '",
                value.bytes(),
                "' must have exactly one element, but has ",
                context.len().to_string()
            ),
            pos,
        ));
    }
    let elem = match context.pop().unwrap() {
        OwnedContext::Opaque(path) => {
            if !path.ends_with(b".drv") { return Err(language.legacy().error_at(ErrorKind::Eval, msg!("path '", language.legacy().config.store_dir.as_str(), "/", &path, "' is not a derivation"), pos)); }
            OwnedContext::DrvDeep(path)
        }
        OwnedContext::DrvDeep(path) => OwnedContext::DrvDeep(path),
        OwnedContext::Built { output, .. } => return Err(language.legacy().error_at(ErrorKind::Eval, msg!("`addDrvOutputDependencies` can only act on derivations, not on a derivation output such as '", output, "'"), pos)),
        OwnedContext::Path(_) | OwnedContext::LazyPath(_) => return Err(language.legacy().error_at(ErrorKind::Eval, "`addDrvOutputDependencies` does not work on a string without context", pos)),
        OwnedContext::LazyHash { .. } => unreachable!("hashes were resolved"),
    };
    recontextualize(machine, roots, value, &[elem])
}

#[derive(Default)]
struct ContextInfo {
    path: bool,
    all_outputs: bool,
    outputs: Vec<Vec<u8>>,
}

fn context_path(language: &Language<'_>, path: &[u8]) -> Result<StorePath> {
    let text = std::str::from_utf8(path).map_err(|_| {
        language
            .legacy()
            .error(ErrorKind::Eval, msg!("invalid store path '", path, "'"))
    })?;
    language.legacy().store_path_of(text)
}

fn get_context<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let value = force_string(
        language,
        machine,
        roots,
        cell,
        pos,
        "while evaluating the argument passed to builtins.getContext",
    )?;
    let value = materialize(language, machine, roots, value)?;
    let context = snapshot_context(machine, value);
    let mut infos: BTreeMap<StorePath, ContextInfo> = BTreeMap::new();
    for elem in context {
        match elem {
            OwnedContext::Opaque(path) => {
                infos
                    .entry(context_path(language, &path)?)
                    .or_default()
                    .path = true
            }
            OwnedContext::DrvDeep(path) => {
                infos
                    .entry(context_path(language, &path)?)
                    .or_default()
                    .all_outputs = true
            }
            OwnedContext::Built { drv, output } => infos
                .entry(context_path(language, &drv)?)
                .or_default()
                .outputs
                .push(output),
            OwnedContext::Path(_) | OwnedContext::LazyPath(_) => {}
            OwnedContext::LazyHash { .. } => unreachable!("hashes were resolved"),
        }
    }
    let path_symbol = language.legacy().sym("path");
    let all_symbol = language.legacy().sym("allOutputs");
    let mut fields = Vec::with_capacity(infos.len());
    for (path, info) in infos {
        let value = machine.with_roots(|machine, temporary| {
            let mut inner = Vec::new();
            if info.path {
                inner.push(attr(machine, temporary, path_symbol, temporary.bool(true))?);
            }
            if info.all_outputs {
                inner.push(attr(machine, temporary, all_symbol, temporary.bool(true))?);
            }
            if !info.outputs.is_empty() {
                let outputs = temporary.vector::<CellKind>();
                for name in info.outputs {
                    machine.with_roots(|machine, item_roots| {
                        let output = allocation(machine.heap.string(item_roots, &name))?;
                        outputs.push(allocation(machine.heap.cell(item_roots, output))?);
                        Ok::<(), Error>(())
                    })?;
                }
                let cells: Vec<_> = (0..outputs.len())
                    .map(|index| outputs.get(temporary, index).unwrap())
                    .collect();
                let value = allocation(machine.heap.list(temporary, &cells))?;
                inner.push(attr(machine, temporary, sym::OUTPUTS, value)?);
            }
            let value = attrs_value(machine, temporary, inner)?;
            Ok::<_, Error>(roots.copy(value))
        })?;
        let name = language
            .legacy()
            .sym(&language.legacy().config.store_dir.display(&path));
        fields.push(attr(machine, roots, name, value)?);
    }
    attrs_value(machine, roots, fields)
}

fn append_context<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let value = force_string(
        language,
        machine,
        roots,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.appendContext",
    )?;
    let value = materialize(language, machine, roots, value)?;
    let mut context = snapshot_context(machine, value);
    let attrs = force_attrs(
        language,
        machine,
        roots,
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.appendContext",
    )?;
    let attrs = managed_bindings::attributes(&machine.heap, roots, attrs);
    let path_symbol = language.legacy().sym("path");
    let all_symbol = language.legacy().sym("allOutputs");
    for outer in attrs {
        machine.with_roots(|machine, temporary| {
            let name = language.legacy().symbol_name(outer.name);
            let parsed = std::str::from_utf8(name)
                .ok()
                .and_then(|name| language.legacy().config.store_dir.maybe_parse(name));
            let path = parsed.ok_or_else(|| {
                language.legacy().error_at(
                    ErrorKind::Eval,
                    msg!(
                        "context key '",
                        language.legacy().symbol_ident(outer.name),
                        "' is not a store path"
                    ),
                    outer.pos,
                )
            })?;
            let base = path.as_str().as_bytes().to_vec();
            let info = force_attrs(
                language,
                machine,
                temporary,
                outer.value,
                outer.pos,
                "while evaluating the value of a string context",
            )?;
            let field = machine
                .heap
                .bindings_view(info)
                .get(path_symbol)
                .map(|attr| attr.root(temporary));
            if let Some(field) = field {
                if force_bool(
                    language,
                    machine,
                    temporary,
                    field.value,
                    field.pos,
                    "while evaluating the `path` attribute of a string context",
                )? {
                    context.push(OwnedContext::Opaque(base.clone()));
                }
            }
            let field = machine
                .heap
                .bindings_view(info)
                .get(all_symbol)
                .map(|attr| attr.root(temporary));
            if let Some(field) = field {
                if force_bool(
                    language,
                    machine,
                    temporary,
                    field.value,
                    field.pos,
                    "while evaluating the `allOutputs` attribute of a string context",
                )? {
                    if !path.is_derivation() {
                        return Err(language.legacy().error_at(
                            ErrorKind::Eval,
                            msg!(
                                "tried to add all-outputs context of ",
                                language.legacy().symbol_ident(outer.name),
                                ", which is not a derivation, to a string"
                            ),
                            outer.pos,
                        ));
                    }
                    context.push(OwnedContext::DrvDeep(base.clone()));
                }
            }
            let field = machine
                .heap
                .bindings_view(info)
                .get(sym::OUTPUTS)
                .map(|attr| attr.root(temporary));
            if let Some(field) = field {
                let outputs = force_list(
                    language,
                    machine,
                    temporary,
                    field.value,
                    field.pos,
                    "while evaluating the `outputs` attribute of a string context",
                )?;
                if !outputs.is_empty() && !path.is_derivation() {
                    return Err(language.legacy().error_at(
                        ErrorKind::Eval,
                        msg!(
                            "tried to add derivation output context of ",
                            language.legacy().symbol_ident(outer.name),
                            ", which is not a derivation, to a string"
                        ),
                        outer.pos,
                    ));
                }
                for cell in outputs {
                    let output = machine.with_roots(|machine, item_roots| {
                        no_context(
                            language,
                            machine,
                            item_roots,
                            cell,
                            field.pos,
                            "while evaluating an output name within a string context",
                        )
                    })?;
                    context.push(OwnedContext::Built {
                        drv: base.clone(),
                        output,
                    });
                }
            }
            Ok::<(), Error>(())
        })?;
    }
    recontextualize(machine, roots, value, &context)
}

pub(crate) fn borrowed_context(
    context: &[OwnedContext],
) -> Result<crate::string_context::StringContext<'_>> {
    use crate::value::ContextElem;
    let text = |bytes| {
        std::str::from_utf8(bytes).map_err(|_| {
            crate::error::EvalError::new(ErrorKind::Eval, "invalid UTF-8 in store path context")
        })
    };
    let mut result = crate::string_context::StringContext::new();
    for entry in context {
        result.insert(match entry {
            OwnedContext::LazyHash { offset, tree } => ContextElem::LazyHash {
                offset: *offset,
                tree: *tree,
            },
            OwnedContext::LazyPath(tree) => ContextElem::LazyPath(*tree),
            OwnedContext::Opaque(path) => ContextElem::Opaque(text(path)?),
            OwnedContext::DrvDeep(path) => ContextElem::DrvDeep(text(path)?),
            OwnedContext::Path(path) => ContextElem::Path(text(path)?),
            OwnedContext::Built { drv, output } => ContextElem::Built {
                drv: text(drv)?,
                output,
            },
        });
    }
    Ok(result)
}

enum JsonOpen<'r, 'h> {
    List(Vec<CellRoot<'r, 'h>>),
    Attrs(Vec<RootedAttr<'r, 'h>>, Option<Symbol>),
}
struct JsonBuilder<'a, 'l, 'r, 'h> {
    language: &'a Language<'l>,
    machine: &'a mut Machine<'h>,
    roots: &'a Roots<'r, 'h>,
    open: Vec<JsonOpen<'r, 'h>>,
    root: Option<ValueRoot<'r, 'h>>,
}
impl<'r, 'h> JsonBuilder<'_, '_, 'r, 'h> {
    fn add(&mut self, value: ValueRoot<'r, 'h>) -> Result<()> {
        match self.open.last_mut() {
            Some(JsonOpen::List(items)) => {
                items.push(allocation(self.machine.heap.cell(self.roots, value))?)
            }
            Some(JsonOpen::Attrs(attrs, name)) => attrs.push(attr(
                self.machine,
                self.roots,
                name.take().expect("object key"),
                value,
            )?),
            None => self.root = Some(value),
        }
        Ok(())
    }
    fn no_null(&self, bytes: &[u8]) -> Result<()> {
        if bytes.contains(&0) {
            return Err(self.language.legacy().error(ErrorKind::Eval, format!("input string '{}' cannot be represented as Nix string because it contains null bytes", String::from_utf8_lossy(bytes).replace('\0', "␀"))));
        }
        Ok(())
    }
}
impl brix_store::json::Sax for JsonBuilder<'_, '_, '_, '_> {
    type Error = Error;
    fn null(&mut self) -> Result<()> {
        self.add(self.roots.null())
    }
    fn boolean(&mut self, value: bool) -> Result<()> {
        self.add(self.roots.bool(value))
    }
    fn number_integer(&mut self, value: i64) -> Result<()> {
        self.add(self.roots.int(value))
    }
    fn number_unsigned(&mut self, value: u64) -> Result<()> {
        let value = i64::try_from(value).map_err(|_| {
            self.language.legacy().error(
                ErrorKind::Eval,
                format!("unsigned json number {value} outside of Nix integer range"),
            )
        })?;
        self.add(self.roots.int(value))
    }
    fn number_float(&mut self, value: f64, _: &[u8]) -> Result<()> {
        self.add(self.roots.float(value))
    }
    fn string(&mut self, bytes: &[u8]) -> Result<()> {
        self.no_null(bytes)?;
        let value = allocation(self.machine.heap.string(self.roots, bytes))?;
        self.add(value)
    }
    fn start_object(&mut self) -> Result<()> {
        self.open.push(JsonOpen::Attrs(Vec::new(), None));
        Ok(())
    }
    fn key(&mut self, key: &[u8]) -> Result<()> {
        self.no_null(key)?;
        let name = self.language.legacy().intern(key);
        if let Some(JsonOpen::Attrs(_, pending)) = self.open.last_mut() {
            *pending = Some(name);
        }
        Ok(())
    }
    fn end_object(&mut self) -> Result<()> {
        if let Some(JsonOpen::Attrs(attrs, _)) = self.open.pop() {
            let value = attrs_value(self.machine, self.roots, attrs)?;
            self.add(value)?;
        }
        Ok(())
    }
    fn start_array(&mut self) -> Result<()> {
        self.open.push(JsonOpen::List(Vec::new()));
        Ok(())
    }
    fn end_array(&mut self) -> Result<()> {
        if let Some(JsonOpen::List(items)) = self.open.pop() {
            let value = allocation(self.machine.heap.list(self.roots, &items))?;
            self.add(value)?;
        }
        Ok(())
    }
}
fn from_json<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let bytes = no_context(
        language,
        machine,
        roots,
        cell,
        pos,
        "while evaluating the first argument passed to builtins.fromJSON",
    )?;
    let mut builder = JsonBuilder {
        language,
        machine,
        roots,
        open: Vec::new(),
        root: None,
    };
    match brix_store::json::sax_parse(&bytes, &mut builder) {
        Ok(()) => Ok(builder.root.expect("parsed document")),
        Err(brix_store::json::SaxError::Parse(error)) => {
            let error = language
                .legacy()
                .error(ErrorKind::Parse, error.message().to_vec());
            language
                .legacy()
                .add_trace(&error, pos, "while decoding a JSON string");
            Err(error)
        }
        Err(brix_store::json::SaxError::Handler(error)) => Err(error),
    }
}

#[derive(Default)]
struct JsonOutput {
    copy_to_store: bool,
    text: String,
    context: Vec<OwnedContext>,
    first_error: Option<String>,
}
impl JsonOutput {
    fn string(&mut self, bytes: &[u8]) {
        if self.first_error.is_none() {
            self.first_error = crate::json::utf8_error(bytes);
        }
        crate::json::push_escaped(&mut self.text, &String::from_utf8_lossy(bytes));
    }
}
fn to_json<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let (text, context, first_error) = json_undumped(language, machine, roots, cell, pos)?;
    let output = JsonOutput {
        copy_to_store: true,
        text,
        context,
        first_error,
    };
    if let Some(error) = output.first_error {
        return Err(crate::error::EvalError::new(
            ErrorKind::Eval,
            format!("JSON serialization error: {error}"),
        ));
    }
    string(machine, roots, output.text.as_bytes(), &output.context)
}
pub(crate) fn json_undumped<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<(String, Vec<OwnedContext>, Option<String>)> {
    json_undumped_mode(language, machine, roots, cell, pos, true)
}

pub(crate) fn json_undumped_mode<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    copy_to_store: bool,
) -> Result<(String, Vec<OwnedContext>, Option<String>)> {
    if language.legacy().config.eval_cores != 1 {
        crate::managed_builtins::deep_force_quiet(language, machine, roots, cell, pos)?;
    }
    let mut output = JsonOutput {
        copy_to_store,
        ..Default::default()
    };
    json_inner(language, machine, cell, pos, &mut output)?;
    Ok((output.text, output.context, output.first_error))
}

fn json_inner<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    output: &mut JsonOutput,
) -> Result<()> {
    language.legacy().with_stack(|| {
        machine.with_roots(|machine, roots| {
            let _depth = language.legacy().enter(pos)?;
            let value = language.force(machine, roots, cell, pos)?;
            match machine.heap.view(value) {
                ValueView::Int(value) => {
                    let _ = write!(output.text, "{value}");
                }
                ValueView::Float(value) => {
                    output.text.push_str(&crate::json::format_json_float(value))
                }
                ValueView::Bool(value) => {
                    output.text.push_str(if value { "true" } else { "false" })
                }
                ValueView::Null => output.text.push_str("null"),
                ValueView::Path(path) if !output.copy_to_store => {
                    let accessor = path.accessor();
                    let bytes = path.bytes().to_vec();
                    let path = crate::value::PathVal {
                        accessor,
                        path: &bytes,
                    };
                    let bytes = language.legacy().path_text(&path)?;
                    output.string(&bytes);
                }
                ValueView::String(_) | ValueView::Path(_) => {
                    let value = crate::managed_builtins::coerce(
                        language, machine, roots, cell, pos, false, true,
                    )?;
                    let value = materialize(language, machine, roots, value)?;
                    extend_context(machine, value, &mut output.context);
                    output.string(bytes(machine, value));
                }
                ValueView::Attrs(set) => {
                    let set = set.root(roots);
                    let to_string = machine
                        .heap
                        .bindings_view(set)
                        .get(sym::TO_STRING)
                        .map(|a| a.root(roots));
                    if let Some(to_string) = to_string {
                        let function = language.force(machine, roots, to_string.value, pos)?;
                        let result = language.call_value(machine, roots, function, &[cell], pos)?;
                        let result = allocation(machine.heap.cell(roots, result))?;
                        let result = coerce_with_context(
                            language,
                            machine,
                            roots,
                            result,
                            pos,
                            Coercion::NO_COPY,
                            "while evaluating the result of the `__toString` attribute",
                        )?;
                        let result = materialize(language, machine, roots, result)?;
                        extend_context(machine, result, &mut output.context);
                        output.string(bytes(machine, result));
                        return Ok(());
                    }
                    let out_path = machine
                        .heap
                        .bindings_view(set)
                        .get(sym::OUT_PATH)
                        .map(|a| a.root(roots));
                    if let Some(out_path) = out_path {
                        return json_inner(language, machine, out_path.value, out_path.pos, output);
                    }
                    let attrs = managed_bindings::lexicographic(
                        &machine.heap,
                        roots,
                        set,
                        language.legacy().keys,
                    );
                    output.text.push('{');
                    for (index, attr) in attrs.into_iter().enumerate() {
                        if index != 0 {
                            output.text.push(',');
                        }
                        output.string(language.legacy().symbol_name(attr.name));
                        output.text.push(':');
                        json_inner(language, machine, attr.value, attr.pos, output).inspect_err(
                            |error| {
                                language.legacy().add_trace(
                                    error,
                                    attr.pos,
                                    msg!(
                                        "while evaluating attribute '",
                                        language.legacy().symbol_ident(attr.name),
                                        "'"
                                    ),
                                );
                            },
                        )?;
                    }
                    output.text.push('}');
                }
                ValueView::List(list) => {
                    let len = list.len();
                    output.text.push('[');
                    for index in 0..len {
                        if index != 0 {
                            output.text.push(',');
                        }
                        let ValueView::List(list) = machine.heap.view(value) else {
                            unreachable!()
                        };
                        let cell = list.get(index).unwrap().root(roots);
                        json_inner(language, machine, cell, pos, output).inspect_err(|error| {
                            language.legacy().add_trace(
                                error,
                                pos,
                                format!("while evaluating list element at index {index}"),
                            );
                        })?;
                    }
                    output.text.push(']');
                }
                view => {
                    let error_pos = match view {
                        ValueView::Lambda { node, .. } => {
                            let (code, lambda) = language.legacy().lambda_def(node);
                            language.legacy().src_pos(code, lambda.pos)
                        }
                        _ => pos,
                    };
                    return Err(language.legacy().error_at(
                        ErrorKind::TypeError,
                        format!(
                            "cannot convert {} to JSON",
                            crate::managed_print::describe_type(language, machine, value)
                        ),
                        error_pos,
                    ));
                }
            }
            Ok(())
        })
    })
}

fn from_toml<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let text = no_context_value(
        language,
        machine,
        roots,
        cell,
        pos,
        "while evaluating the argument passed to builtins.fromTOML",
    )?;
    let value = toml::from_str::<toml::Value>(&String::from_utf8_lossy(bytes(machine, text)))
        .map_err(|error| {
            language.legacy().error_at(
                ErrorKind::Eval,
                format!("while parsing TOML: {}", error.message()),
                pos,
            )
        })?;
    toml_value(language, machine, roots, value, pos)
}
fn toml_value<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    value: toml::Value,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    language.legacy().with_stack(|| machine.with_roots(|machine, temporary| {
        let no_nul = |text: &str| -> Result<()> {
            if text.contains('\0') { return Err(language.legacy().error_at(ErrorKind::Eval, format!("while parsing TOML: error: input string '{}' cannot be represented as Nix string because it contains null bytes", text.replace('\0', "␀")), pos)); }
            Ok(())
        };
        let result = match value {
            toml::Value::Integer(value) => temporary.int(value),
            toml::Value::Float(value) => temporary.float(value),
            toml::Value::Boolean(value) => temporary.bool(value),
            toml::Value::String(value) => { no_nul(&value)?; allocation(machine.heap.string(temporary, value.as_bytes()))? }
            toml::Value::Array(values) => {
                let items = temporary.vector::<CellKind>();
                for value in values {
                    machine.with_roots(|machine, element| -> Result<()> {
                        let value = toml_value(language, machine, element, value, pos)?;
                        items.push(allocation(machine.heap.cell(element, value))?);
                        Ok(())
                    })?;
                }
                let items: Vec<_> = (0..items.len()).map(|i| items.get(temporary, i).unwrap()).collect();
                allocation(machine.heap.list(temporary, &items))?
            }
            toml::Value::Table(values) => {
                let mut attrs = Vec::with_capacity(values.len());
                for (name, value) in values {
                    no_nul(&name)?;
                    let value = toml_value(language, machine, temporary, value, pos)?;
                    attrs.push(attr(machine, temporary, language.legacy().sym(&name), value)?);
                }
                attrs_value(machine, temporary, attrs)?
            }
            toml::Value::Datetime(value) => {
                if !language.legacy().experimental("parse-toml-timestamps") { return Err(language.legacy().error_at(ErrorKind::Eval, "while parsing TOML: Dates and times are not supported", pos)); }
                let kind = allocation(machine.heap.string(temporary, b"timestamp"))?;
                let value = normalize_toml_datetime(&value.to_string());
                let value = allocation(machine.heap.string(temporary, value.as_bytes()))?;
                let attrs = vec![attr(machine, temporary, language.legacy().sym("_type"), kind)?, attr(machine, temporary, sym::VALUE, value)?];
                attrs_value(machine, temporary, attrs)?
            }
        };
        Ok(roots.copy(result))
    }))
}
fn to_xml<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let mut output = XmlWriter::new();
    let mut context = Vec::new();
    let mut seen = BTreeSet::new();
    output.open_element("expr", &[]);
    xml_inner(
        language,
        machine,
        cell,
        pos,
        &mut output,
        &mut context,
        &mut seen,
    )?;
    output.close_element();
    string(machine, roots, &output.out, &context)
}
fn xml_inner<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    output: &mut XmlWriter,
    context: &mut Vec<OwnedContext>,
    seen: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    language.legacy().with_stack(|| {
        machine.with_roots(|machine, roots| {
            let _depth = language.legacy().enter(pos)?;
            let value = language.force(machine, roots, cell, pos)?;
            match machine.heap.view(value) {
                ValueView::Int(value) => {
                    output.empty_element("int", &[("value", value.to_string().as_bytes())])
                }
                ValueView::Float(value) => output.empty_element(
                    "float",
                    &[("value", brix_syntax::float::format_g(value).as_bytes())],
                ),
                ValueView::Bool(value) => output
                    .empty_element("bool", &[("value", if value { b"true" } else { b"false" })]),
                ValueView::Null => output.empty_element("null", &[]),
                ValueView::String(_) => {
                    let value = materialize(language, machine, roots, value)?;
                    extend_context(machine, value, context);
                    output.empty_element("string", &[("value", bytes(machine, value))]);
                }
                ValueView::Path(path) => {
                    let path = language.legacy().accessor(path.accessor()).show_path(
                        &brix_store::CanonPath::new(path.bytes()).expect("canonical managed path"),
                    );
                    output.empty_element("path", &[("value", &path)]);
                }
                ValueView::List(list) => {
                    let len = list.len();
                    output.open_element("list", &[]);
                    for index in 0..len {
                        let ValueView::List(list) = machine.heap.view(value) else {
                            unreachable!()
                        };
                        let cell = list.get(index).unwrap().root(roots);
                        xml_inner(language, machine, cell, pos, output, context, seen)?;
                    }
                    output.close_element();
                }
                ValueView::Attrs(set) => {
                    let set = set.root(roots);
                    let kind = machine
                        .heap
                        .bindings_view(set)
                        .get(sym::TYPE)
                        .map(|attr| attr.root(roots));
                    let mut derivation = false;
                    if let Some(kind) = kind {
                        let kind = language.force(machine, roots, kind.value, kind.pos)?;
                        if matches!(machine.heap.view(kind), ValueView::String(_)) {
                            let kind = materialize(language, machine, roots, kind)?;
                            derivation = bytes(machine, kind) == b"derivation";
                        }
                    }
                    let show_attributes = if derivation {
                        // Keep value roots across evaluation of the other field;
                        // the XML writer consumes borrowed bytes without collecting.
                        let mut xml_attrs = smallvec::SmallVec::<[_; 2]>::new();
                        for (name, symbol) in
                            [("drvPath", sym::DRV_PATH), ("outPath", sym::OUT_PATH)]
                        {
                            let field = machine
                                .heap
                                .bindings_view(set)
                                .get(symbol)
                                .map(|a| a.root(roots));
                            if let Some(field) = field {
                                let value =
                                    language.force(machine, roots, field.value, field.pos)?;
                                if matches!(machine.heap.view(value), ValueView::String(_)) {
                                    let value = materialize(language, machine, roots, value)?;
                                    xml_attrs.push((name, value));
                                }
                            }
                        }
                        let borrowed: smallvec::SmallVec<[_; 2]> = xml_attrs
                            .iter()
                            .map(|(name, value)| (*name, bytes(machine, *value)))
                            .collect();
                        output.open_element("derivation", &borrowed);
                        xml_attrs
                            .iter()
                            .find(|(name, _)| *name == "drvPath")
                            .is_some_and(|(_, value)| {
                                let path = bytes(machine, *value);
                                !path.is_empty() && seen.insert(path.to_vec())
                            })
                    } else {
                        output.open_element("attrs", &[]);
                        true
                    };
                    if show_attributes {
                        let attrs = managed_bindings::lexicographic(
                            &machine.heap,
                            roots,
                            set,
                            language.legacy().keys,
                        );
                        for attr in attrs {
                            output.open_element(
                                "attr",
                                &[("name", language.legacy().symbol_name(attr.name))],
                            );
                            xml_inner(
                                language, machine, attr.value, attr.pos, output, context, seen,
                            )?;
                            output.close_element();
                        }
                    } else {
                        output.empty_element("repeated", &[]);
                    }
                    output.close_element();
                }
                ValueView::Lambda { node, .. } => {
                    let (_, lambda) = language.legacy().lambda_def(node);
                    output.open_element("function", &[]);
                    if let Some(formals) = &lambda.formals {
                        let mut pattern = Vec::new();
                        if let Some(arg) = lambda.arg {
                            pattern.push(("name", language.legacy().symbol_name(arg)));
                        }
                        if formals.ellipsis {
                            pattern.push(("ellipsis", b"1".as_slice()));
                        }
                        output.open_element("attrspat", &pattern);
                        let mut fields: Vec<_> = formals.formals.iter().collect();
                        fields.sort_by(|left, right| {
                            language
                                .legacy()
                                .symbol_name(left.name)
                                .cmp(language.legacy().symbol_name(right.name))
                        });
                        for field in fields {
                            output.empty_element(
                                "attr",
                                &[("name", language.legacy().symbol_name(field.name))],
                            );
                        }
                        output.close_element();
                    } else {
                        output.empty_element(
                            "varpat",
                            &[(
                                "name",
                                language
                                    .legacy()
                                    .symbol_name(lambda.arg.expect("plain lambda argument")),
                            )],
                        );
                    }
                    output.close_element();
                }
                _ => output.empty_element("unevaluated", &[]),
            }
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;

    fn check(sources: &[&str]) {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                workers: 0,
                stress: true,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 1024,
                chunk_bytes: 256,
            },
            |heap| {
                let mut machine = Machine::new(heap);
                for &source in sources {
                    machine.with_roots(|machine, roots| {
                        let value = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .unwrap_or_else(|error| panic!("{source}: {}", error.render(true)));
                        assert!(
                            matches!(machine.heap.view(value), ValueView::Bool(true)),
                            "{source}: {}",
                            String::from_utf8_lossy(&crate::managed_builtins::print_for_error(
                                &language, machine, value
                            ))
                        );
                        machine.heap.collect().unwrap();
                        assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                    });
                }
                assert!(machine.heap.stats().collections > 0);
            },
        )
        .unwrap();
    }

    #[test]
    fn string_errors_match_legacy() {
        for source in [
            "toString {}",
            "builtins.stringLength 1",
            "builtins.substring 0 1 {}",
            "builtins.unsafeDiscardStringContext {}",
            "builtins.unsafeDiscardOutputDependency {}",
            "builtins.addDrvOutputDependencies {}",
            "builtins.concatStringsSep \":\" [ {} ]",
            "builtins.fromJSON \"[1,]\"",
            "builtins.match \"[\" \"x\"",
            "builtins.toJSON (x: x)",
            r#""${throw "first"}${throw "later"}""#,
            r#""${{}}${throw "later"}""#,
            r#"builtins.concatStringsSep (throw "separator") (throw "list")"#,
            r#"builtins.concatStringsSep ":" [ {} (throw "later") ]"#,
            r#"toString [ [ (throw "first") ] (throw "later") ]"#,
        ] {
            let old_arena = crate::Arena::new();
            let new_arena = crate::Arena::new();
            let host = crate::NullHost;
            let legacy = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
            let expected = legacy
                .eval_str(source.as_bytes(), "/")
                .expect_err("legacy error");
            let language = Language::new(crate::Evaluator::new(
                &new_arena,
                crate::Config::default(),
                &host,
            ));
            with_owned_heap(
                brix_gc::Config {
                    stress: true,
                    ..Default::default()
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|machine, roots| {
                        let actual = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .expect_err("managed error");
                        assert_eq!(actual.render(true), expected.render(true), "{source}");
                    });
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn no_context_roots_preserve_byte_names_and_hidden_context_across_collection() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    // Unregistered LazyPath must remain hidden and must not fetch or hash.
                    let context = [
                        OwnedContext::LazyPath(999),
                        OwnedContext::Path(b"untracked".to_vec()),
                    ];
                    let input = string(machine, roots, b"name\xff", &context).unwrap();
                    let cell = allocation(machine.heap.cell(roots, input)).unwrap();
                    let before = machine.heap.used_bytes();
                    let value = no_context_value(
                        &language,
                        machine,
                        roots,
                        cell,
                        SrcPos::NONE,
                        "byte name",
                    )
                    .unwrap();
                    assert_eq!(
                        bytes(machine, value).as_ptr(),
                        bytes(machine, input).as_ptr()
                    );
                    assert_eq!(machine.heap.used_bytes(), before);
                    machine.heap.collect().unwrap();
                    assert_eq!(bytes(machine, value), b"name\xff");
                    assert_eq!(snapshot(machine, value).1, context);
                    assert_eq!(
                        no_context(&language, machine, roots, cell, SrcPos::NONE, "byte name")
                            .unwrap(),
                        b"name\xff"
                    );
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn no_context_errors_preserve_first_real_entry_and_byte_output_names() {
        let path = b"00000000000000000000000000000000-test.drv".to_vec();
        let cases = [
            vec![OwnedContext::Opaque(path.clone())],
            vec![OwnedContext::DrvDeep(path.clone())],
            vec![OwnedContext::Built {
                drv: path.clone(),
                output: b"out\xff".to_vec(),
            }],
            vec![
                OwnedContext::Built {
                    drv: path.clone(),
                    output: b"out".to_vec(),
                },
                OwnedContext::DrvDeep(path.clone()),
                OwnedContext::Opaque(path),
            ],
        ];
        for context in cases {
            let old_arena = crate::Arena::new();
            let new_arena = crate::Arena::new();
            let host = crate::NullHost;
            let legacy = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
            let borrowed = borrowed_context(&context).unwrap();
            let old = legacy.alloc_string_value(b"name\xff", borrowed.as_slice());
            let old = legacy.alloc_cell(old);
            let expected = legacy
                .force_string_no_ctx(old, SrcPos::NONE, "context check")
                .unwrap_err();
            let language = Language::new(crate::Evaluator::new(
                &new_arena,
                crate::Config::default(),
                &host,
            ));
            with_owned_heap(
                brix_gc::Config {
                    stress: true,
                    ..Default::default()
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|machine, roots| {
                        let value = string(machine, roots, b"name\xff", &context).unwrap();
                        let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                        let error = no_context_value(
                            &language,
                            machine,
                            roots,
                            cell,
                            SrcPos::NONE,
                            "context check",
                        )
                        .unwrap_err();
                        assert_eq!(error.render(true), expected.render(true));
                        let owned_error = no_context(
                            &language,
                            machine,
                            roots,
                            cell,
                            SrcPos::NONE,
                            "context check",
                        )
                        .unwrap_err();
                        assert_eq!(owned_error.render(true), expected.render(true));
                    });
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn comparisons_use_lazy_hash_identity_without_materializing() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    // No such tree exists: attempting to hash it would panic.
                    let context = [OwnedContext::LazyHash {
                        offset: 2,
                        tree: 999,
                    }];
                    let bytes = b"xx00000000000000000000000000000000yy";
                    let left = string(machine, roots, bytes, &context).unwrap();
                    let left = allocation(machine.heap.cell(roots, left)).unwrap();
                    let right = string(machine, roots, bytes, &context).unwrap();
                    let right = allocation(machine.heap.cell(roots, right)).unwrap();
                    assert!(crate::managed_builtins::equal(
                        &language,
                        machine,
                        roots,
                        left,
                        right,
                        SrcPos::NONE
                    )
                    .unwrap());
                    let longer = string(
                        machine,
                        roots,
                        &[bytes.as_slice(), b"/sub"].concat(),
                        &context,
                    )
                    .unwrap();
                    let longer = allocation(machine.heap.cell(roots, longer)).unwrap();
                    assert!(crate::managed_builtins::compare(
                        &language,
                        machine,
                        roots,
                        left,
                        longer,
                        SrcPos::NONE,
                        ""
                    )
                    .unwrap());
                    let different = allocation(
                        machine
                            .heap
                            .string(roots, b"xx!0000000000000000000000000000000yy"),
                    )
                    .unwrap();
                    let different = allocation(machine.heap.cell(roots, different)).unwrap();
                    assert!(!crate::managed_builtins::equal(
                        &language,
                        machine,
                        roots,
                        left,
                        different,
                        SrcPos::NONE
                    )
                    .unwrap());
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn complete_lazy_hash_survives_slicing_and_empty_slice_never_materializes() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    // This deliberately nonexistent tree proves these operations never hash it.
                    let context = [
                        OwnedContext::LazyHash {
                            offset: 2,
                            tree: 999,
                        },
                        OwnedContext::LazyPath(999),
                    ];
                    let value = string(
                        machine,
                        roots,
                        b"xx00000000000000000000000000000000yy",
                        &context,
                    )
                    .unwrap();
                    let mut joined = b"prefix:".to_vec();
                    let mut joined_context = Vec::new();
                    append_snapshot(machine, value, &mut joined, &mut joined_context).unwrap();
                    assert_eq!(joined, b"prefix:xx00000000000000000000000000000000yy");
                    assert_eq!(
                        joined_context,
                        vec![
                            OwnedContext::LazyHash {
                                offset: 9,
                                tree: 999
                            },
                            OwnedContext::LazyPath(999),
                        ]
                    );
                    let joined = string(machine, roots, &joined, &joined_context).unwrap();
                    assert_eq!(snapshot(machine, joined).1, joined_context);
                    let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                    let discarded = context_builtin(
                        &language,
                        machine,
                        roots,
                        "unsafeDiscardStringContext",
                        &[cell],
                        SrcPos::NONE,
                    )
                    .unwrap();
                    machine.heap.collect().unwrap();
                    assert_eq!(snapshot(machine, discarded), snapshot(machine, value));
                    let start = allocation(machine.heap.cell(roots, roots.int(2))).unwrap();
                    let length = allocation(machine.heap.cell(roots, roots.int(32))).unwrap();
                    let sliced = substring(
                        &language,
                        machine,
                        roots,
                        &[start, length, cell],
                        SrcPos::NONE,
                    )
                    .unwrap();
                    machine.heap.collect().unwrap();
                    assert_eq!(
                        snapshot(machine, sliced).0,
                        b"00000000000000000000000000000000"
                    );
                    assert_eq!(
                        snapshot(machine, sliced).1,
                        vec![
                            OwnedContext::LazyHash {
                                offset: 0,
                                tree: 999
                            },
                            OwnedContext::LazyPath(999)
                        ]
                    );
                    let inside = allocation(machine.heap.cell(roots, roots.int(10))).unwrap();
                    let empty = allocation(machine.heap.cell(roots, roots.int(0))).unwrap();
                    let sliced = substring(
                        &language,
                        machine,
                        roots,
                        &[inside, empty, cell],
                        SrcPos::NONE,
                    )
                    .unwrap();
                    assert_eq!(
                        snapshot(machine, sliced),
                        (Vec::new(), vec![OwnedContext::LazyPath(999)])
                    );
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn nested_list_coercion_shifts_lazy_contexts_without_materializing() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let text = b"xx00000000000000000000000000000000yy";
                    // Nonexistent tree IDs ensure coercion never asks the host to
                    // materialize a hash. Nested/empty lists exercise separator offsets.
                    let context = [
                        OwnedContext::LazyHash {
                            offset: 2,
                            tree: 999,
                        },
                        OwnedContext::LazyPath(999),
                    ];
                    let value = string(machine, roots, text, &context).unwrap();
                    let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                    let nested = allocation(machine.heap.list(roots, &[cell])).unwrap();
                    let nested = allocation(machine.heap.cell(roots, nested)).unwrap();
                    let empty = allocation(machine.heap.list(roots, &[])).unwrap();
                    let empty = allocation(machine.heap.cell(roots, empty)).unwrap();
                    let list =
                        allocation(machine.heap.list(roots, &[cell, empty, nested])).unwrap();
                    let list = allocation(machine.heap.cell(roots, list)).unwrap();
                    let result = coerce_with_context(
                        &language,
                        machine,
                        roots,
                        list,
                        SrcPos::NONE,
                        Coercion::MORE_COPY,
                        "",
                    )
                    .unwrap();
                    let mut expected = text.to_vec();
                    expected.push(b' ');
                    expected.extend_from_slice(text);
                    for _ in 0..2 {
                        assert_eq!(bytes(machine, result), expected);
                        assert_eq!(
                            snapshot_context(machine, result),
                            vec![
                                OwnedContext::LazyHash {
                                    offset: 2,
                                    tree: 999
                                },
                                OwnedContext::LazyHash {
                                    offset: text.len() as u32 + 3,
                                    tree: 999
                                },
                                OwnedContext::LazyPath(999),
                            ]
                        );
                        machine.heap.collect().unwrap();
                    }
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn concatenated_parts_keep_lazy_hashes_context_offsets_and_bytes_through_moves() {
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let left = string(machine, roots, b"prefix\xff", &[]).unwrap();
                    let hash = string(
                        machine,
                        roots,
                        b"xx00000000000000000000000000000000yy",
                        &[
                            OwnedContext::LazyHash {
                                offset: 2,
                                tree: 999,
                            },
                            OwnedContext::LazyPath(999),
                            OwnedContext::Opaque(b"00000000000000000000000000000000-test".to_vec()),
                        ],
                    )
                    .unwrap();
                    let parts = [left, hash, left, hash];
                    let mut context = Vec::new();
                    let mut segments = ContextSegments::new();
                    let mut prefix = 0;
                    for part in parts {
                        append_context_segment(machine, part, prefix, &mut segments).unwrap();
                        prefix =
                            append_context_snapshot(machine, part, prefix, &mut context).unwrap();
                    }
                    let expected = [
                        bytes(machine, left),
                        bytes(machine, hash),
                        bytes(machine, left),
                        bytes(machine, hash),
                    ]
                    .concat();
                    let joined = concat_parts(machine, roots, &parts, &segments).unwrap();
                    machine.heap.collect().unwrap();
                    assert_eq!(bytes(machine, joined), expected);
                    assert_eq!(bytes(machine, joined).len(), prefix);
                    let (_, actual_context) = snapshot(machine, joined);
                    let mut expected_context = context;
                    expected_context.sort();
                    expected_context.dedup();
                    assert_eq!(actual_context, expected_context);
                    // Check offsets during each append, without needing a >4GiB fixture.
                    let mut rejected_context = ContextSegments::new();
                    let error = append_context_segment(
                        machine,
                        hash,
                        u32::MAX as usize,
                        &mut rejected_context,
                    )
                    .unwrap_err();
                    assert!(error
                        .render(false)
                        .windows(b"string is too large".len())
                        .any(|part| part == b"string is too large"));
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn slices_and_context_changes_share_large_payloads_across_collection() {
        const LEN: usize = 1024 * 1024;
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        let payload: Vec<u8> = (0..LEN).map(|index| (index % 251) as u8).collect();
        for stress in [false, true] {
            with_owned_heap(
                brix_gc::Config {
                    steady_threshold_bytes: None,
                    trace_collections: false,
                    threshold_bytes: 1024 * 1024 * 1024,
                    stress,
                    ..Default::default()
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|machine, roots| {
                        let slices = roots.vector::<crate::managed_heap::ValueKind>();
                        machine.with_roots(|machine, source_roots| {
                            let source = string(
                                machine,
                                source_roots,
                                &payload,
                                &[OwnedContext::Opaque(
                                    b"00000000000000000000000000000000-source.drv".to_vec(),
                                )],
                            )
                            .unwrap();
                            let source_cell =
                                allocation(machine.heap.cell(source_roots, source)).unwrap();
                            let length = allocation(
                                machine
                                    .heap
                                    .cell(source_roots, source_roots.int((LEN - 128) as i64)),
                            )
                            .unwrap();
                            let empty = managed_bindings::empty(
                                &mut machine.heap,
                                source_roots,
                                SrcPos::NONE,
                            )
                            .unwrap();
                            let empty = machine.heap.bindings_value(source_roots, empty);
                            let empty = allocation(machine.heap.cell(source_roots, empty)).unwrap();
                            let before = machine.heap.used_bytes();
                            for index in 0..16 {
                                machine.with_roots(|machine, temporary| {
                                    let start = allocation(
                                        machine.heap.cell(temporary, temporary.int(index + 1)),
                                    )
                                    .unwrap();
                                    let sliced = substring(
                                        &language,
                                        machine,
                                        temporary,
                                        &[
                                            start,
                                            temporary.copy(length),
                                            temporary.copy(source_cell),
                                        ],
                                        SrcPos::NONE,
                                    )
                                    .unwrap();
                                    slices.push(sliced);
                                    for name in [
                                        "unsafeDiscardStringContext",
                                        "unsafeDiscardOutputDependency",
                                        "addDrvOutputDependencies",
                                        "appendContext",
                                    ] {
                                        let changed = context_builtin(
                                            &language,
                                            machine,
                                            temporary,
                                            name,
                                            &[source_cell, empty],
                                            SrcPos::NONE,
                                        )
                                        .unwrap();
                                        let (
                                            ValueView::String(original),
                                            ValueView::String(changed),
                                        ) = (machine.heap.view(source), machine.heap.view(changed))
                                        else {
                                            panic!("strings")
                                        };
                                        assert_eq!(
                                            original.bytes().as_ptr(),
                                            changed.bytes().as_ptr(),
                                            "{name} must share bytes"
                                        );
                                        assert_eq!(changed.bytes(), payload);
                                    }
                                });
                            }
                            // Without collection, every abandoned header is still
                            // counted. Copying even one payload exceeds this bound.
                            if !stress {
                                assert!(machine.heap.used_bytes() - before < 64 * 1024);
                            }
                        });
                        // The source header and all whole-string transforms are
                        // gone. Interior slice roots alone must retain the bytes.
                        machine.heap.collect().unwrap();
                        for index in 0..16 {
                            let value = slices.get(roots, index).unwrap();
                            let ValueView::String(value) = machine.heap.view(value) else {
                                panic!("string")
                            };
                            assert_eq!(value.bytes(), &payload[index + 1..index + 1 + LEN - 128]);
                        }
                    });
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn strings_under_collection_at_every_allocation() {
        check(&[
            r#"toString [ 1 true null "x" ] == "1 1  x""#,
            r#"builtins.substring 2 3 "abcdef" == "cde""#,
            r#"builtins.substring 2 (-1) "abcdef" == "cdef""#,
            r#"builtins.substring 99 3 "abcdef" == """#,
            r#"builtins.stringLength "hello" == 5"#,
            r#"builtins.hashString "sha256" "abc" == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad""#,
            r#"builtins.convertHash { hash = builtins.hashString "sha256" "abc"; hashAlgo = "sha256"; toHashFormat = "base16"; } == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad""#,
            r#"builtins.match "(a)(b)?" "a" == [ "a" null ]"#,
            r#"builtins.match "a" "ba" == null"#,
            r#"builtins.split "([ac])" "abc" == [ "" [ "a" ] "b" [ "c" ] "" ]"#,
            r#"builtins.split "" "ab" == [ "" [] "a" [] "b" [] "" ]"#,
            r#"builtins.concatStringsSep ":" [ "a" "b" "c" ] == "a:b:c""#,
            r#"builtins.replaceStrings [ "a" "never" ] [ "x" (throw "unused") ] "aba" == "xbx""#,
            r#"builtins.replaceStrings [ "" ] [ "." ] "ab" == ".a.b.""#,
            r#"builtins.parseDrvName "hello-1.2" == { name = "hello"; version = "1.2"; }"#,
            r#"builtins.compareVersions "1.2" "1.10" == -1"#,
            r#"builtins.splitVersion "1.2pre-abc" == [ "1" "2" "pre" "abc" ]"#,
        ]);
    }

    #[test]
    fn contexts_under_collection_at_every_allocation() {
        check(&[
            r#"let p = "/nix/store/00000000000000000000000000000000-test.drv"; s = builtins.appendContext "abc" { ${p} = { path = true; outputs = [ "out" "dev" ]; allOutputs = true; }; }; in builtins.getContext s == { ${p} = { path = true; outputs = [ "dev" "out" ]; allOutputs = true; }; }"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "abc" { ${p}.path = true; }; in builtins.hasContext s && !(builtins.hasContext (builtins.unsafeDiscardStringContext s))"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "abc" { ${p}.path = true; }; in builtins.getContext (builtins.substring 0 0 s) == builtins.getContext s"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test.drv"; s = builtins.appendContext "abc" { ${p}.path = true; }; in builtins.getContext (builtins.addDrvOutputDependencies s) == { ${p}.allOutputs = true; }"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test.drv"; s = builtins.appendContext "abc" { ${p}.allOutputs = true; }; in builtins.getContext (builtins.unsafeDiscardOutputDependency s) == { ${p}.path = true; }"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "x" { ${p}.path = true; }; in builtins.getContext (builtins.concatStringsSep s []) == builtins.getContext s"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "x" { ${p}.path = true; }; in !(builtins.hasContext (builtins.replaceStrings [ "unmatched" ] [ s ] "text"))"#,
        ]);
    }

    #[test]
    fn json_without_copying_paths_uses_text_and_no_store_context() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(brix_gc::Config { stress: true, ..Default::default() }, |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                let value = language.eval_str(machine, roots, b"{ p = /brix-nonexistent-json-path; nested = [ /brix-another-json-path ]; }", "/").unwrap();
                let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                let (text, context, error) = json_undumped_mode(&language, machine, roots, cell, SrcPos::NONE, false).unwrap();
                assert_eq!(text, r#"{"nested":["/brix-another-json-path"],"p":"/brix-nonexistent-json-path"}"#);
                assert!(context.is_empty());
                assert!(error.is_none());
            });
        }).unwrap();
    }

    #[test]
    fn serializers_under_collection_at_every_allocation() {
        check(&[
            r#"builtins.fromJSON ''{"a":[1,true,null,1.5],"a":2}'' == { a = 2; }"#,
            r#"builtins.toJSON { z = [ 1 true null 1.5 ]; a = "text"; } == ''{"a":"text","z":[1,true,null,1.5]}''"#,
            r#"builtins.toJSON { __toString = self: "custom"; other = throw "unused"; } == ''"custom"''"#,
            r#"builtins.toJSON { outPath = [ 1 2 ]; other = throw "unused"; } == "[1,2]""#,
            r#"builtins.fromTOML "[a]\nx = [1, 2]\ns = 'hello'\n" == { a = { x = [ 1 2 ]; s = "hello"; }; }"#,
            r#"builtins.toXML [ 1 "<&" null ] == "<?xml version='1.0' encoding='utf-8'?>\n<expr>\n  <list>\n    <int value=\"1\" />\n    <string value=\"&lt;&amp;\" />\n    <null />\n  </list>\n</expr>\n""#,
            r#"builtins.toXML (x: x) == "<?xml version='1.0' encoding='utf-8'?>\n<expr>\n  <function>\n    <varpat name=\"x\" />\n  </function>\n</expr>\n""#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "abc" { ${p}.path = true; }; in builtins.getContext (builtins.toJSON [ s ]) == builtins.getContext s && builtins.getContext (builtins.toXML [ s ]) == builtins.getContext s"#,
            r#"let p = "/nix/store/00000000000000000000000000000000-test"; s = builtins.appendContext "abc" { ${p}.path = true; }; t = { __toString = _: s; unused = throw "unused"; }; in builtins.toJSON t == ''"abc"'' && builtins.getContext (builtins.toJSON t) == builtins.getContext s && toString [s [] [s]] == "abc abc" && builtins.getContext (toString [s [] [s]]) == builtins.getContext s"#,
        ]);
    }

    #[test]
    fn xml_derivation_metadata_roots_survive_later_field_evaluation() {
        let source = r#"let d = { type = "derivation"; drvPath = builtins.concatStringsSep "" ["/drv" "path"]; outPath = builtins.concatStringsSep "" ["/out" "path"]; a = "payload"; }; in builtins.toXML [d d]"#;
        let old_arena = crate::Arena::new();
        let new_arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
        let expected = legacy.eval_str(source.as_bytes(), "/").unwrap();
        let crate::value::Value::String(expected) = expected else {
            panic!("XML string")
        };
        let expected = legacy.str_bytes(expected).unwrap().to_vec();
        let language = Language::new(crate::Evaluator::new(
            &new_arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(
            brix_gc::Config {
                stress: true,
                ..Default::default()
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let value = language
                        .eval_str(machine, roots, source.as_bytes(), "/")
                        .unwrap();
                    assert_eq!(bytes(machine, value), expected);
                    machine.heap.collect().unwrap();
                    assert_eq!(bytes(machine, value), expected);
                });
            },
        )
        .unwrap();
    }
}
