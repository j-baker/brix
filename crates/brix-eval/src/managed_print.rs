//! Nix-compatible printers and owned snapshots for rooted runtime values.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;

use brix_syntax::float::format_g;
use brix_syntax::printer::{print_attribute_name, print_identifier, print_literal_string};
use bstr::BString;

use crate::error::{msg, ErrorKind};
use crate::managed_bindings as bindings;
use crate::managed_builtins::coerce;
use crate::managed_builtins_strings::{self as strings, OwnedContext};
use crate::managed_eval::Machine;
use crate::managed_heap::{BindingsRoot, CellRoot, Identity, Roots, ValueRoot, ValueView, Visited};
use crate::managed_language::Language;
use crate::owned::OwnedValue;
use crate::print::PrintOptions;
use crate::symbol::sym;
use crate::value::{PathVal, SrcPos};
use crate::Result;

fn push(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

fn position<'h>(l: &Language<'_>, m: &Machine<'h>, value: ValueRoot<'_, 'h>) -> SrcPos {
    match m.heap.view(value) {
        ValueView::Attrs(attrs) => attrs.pos(),
        ValueView::Lambda { node, .. } => {
            let (code, lambda) = l.legacy().lambda_def(node);
            l.legacy().src_pos(code, lambda.pos)
        }
        _ => SrcPos::NONE,
    }
}

/// Resolving lazy text changes host metadata only, never the managed heap.
fn text<'h>(l: &Language<'_>, m: &Machine<'h>, value: ValueRoot<'_, 'h>) -> Result<Vec<u8>> {
    let (mut bytes, context) = strings::snapshot(m, value);
    for entry in context {
        if let OwnedContext::LazyHash { offset, tree } = entry {
            let path = l.legacy().materialize(crate::lazy_tree::LazyTreeId(tree))?;
            let offset = offset as usize;
            bytes[offset..offset + crate::lazy_tree::HASH_LEN]
                .copy_from_slice(path.hash_part().as_bytes());
        }
    }
    Ok(bytes)
}

fn partial_primop_name<'a, 'h>(
    l: &Language<'a>,
    m: &Machine<'h>,
    r: &Roots<'_, 'h>,
    value: ValueRoot<'_, 'h>,
) -> Option<&'a str> {
    let ValueView::PrimOpApp(app) = m.heap.view(value) else {
        return None;
    };
    let mut cell = app.fun().root(r);
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(m.heap.cell_identity(cell)) {
            return None;
        }
        match m.heap.read(cell) {
            ValueView::PrimOp(id) => return Some(l.primop_name(id)),
            ValueView::PrimOpApp(app) => cell = app.fun().root(r),
            _ => return None,
        }
    }
}

pub(crate) fn describe_type<'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
) -> String {
    match m.heap.view(value) {
        ValueView::PrimOp(id) => format!("the built-in function '{}'", l.primop_name(id)),
        ValueView::PrimOpApp(_) => m.heap.root_registry().with_roots(|r| {
            format!(
                "the partially applied built-in function '{}'",
                partial_primop_name(l, m, r, value).unwrap_or("")
            )
        }),
        other => crate::managed_builtins::show_type(other).to_owned(),
    }
}

/// Scalar formatting shared by the collecting and borrowed printers.
fn scalar<'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    r: &Roots<'_, 'h>,
    value: ValueRoot<'_, 'h>,
    out: &mut Vec<u8>,
    ambiguous: bool,
    max_string: usize,
) -> Result<bool> {
    match m.heap.view(value) {
        ValueView::Int(n) => write!(out, "{n}").expect("Vec write"),
        ValueView::Float(n) => push(out, &format_g(n)),
        ValueView::Bool(b) => push(out, if b { "true" } else { "false" }),
        ValueView::Null => push(out, "null"),
        ValueView::String(_) => print_literal_string(out, &text(l, m, value)?, max_string),
        ValueView::Path(path) => out.extend_from_slice(&l.legacy().show_path(&PathVal {
            accessor: path.accessor(),
            path: path.bytes(),
        })),
        ValueView::Lambda { node, .. } => {
            if ambiguous {
                push(out, "<LAMBDA>");
            } else {
                push(out, "«lambda");
                let (code, lambda) = l.legacy().lambda_def(node);
                if let Some(name) = lambda.name {
                    out.push(b' ');
                    out.extend_from_slice(l.legacy().symbol_name(name));
                }
                push(out, " @ ");
                match l.legacy().position(l.legacy().src_pos(code, lambda.pos)) {
                    Some(pos) => out.extend_from_slice(&pos.display()),
                    None => push(out, "«none»:0"),
                }
                push(out, "»");
            }
        }
        ValueView::PrimOp(id) => {
            if ambiguous {
                push(out, "<PRIMOP>");
            } else {
                push(out, &format!("«primop {}»", l.primop_name(id)));
            }
        }
        ValueView::PrimOpApp(_) => {
            if ambiguous {
                push(out, "<PRIMOP-APP>");
            } else {
                let name = partial_primop_name(l, m, r, value);
                match name {
                    Some(name) => push(out, &format!("«partially applied primop {name}»")),
                    None => push(out, "«partially applied primop»"),
                }
            }
        }
        ValueView::Blackhole => push(out, "«potential infinite recursion»"),
        ValueView::Thunk { .. } | ValueView::App(_) => {
            push(out, if ambiguous { "<CODE>" } else { "«thunk»" })
        }
        ValueView::Failed(_) => push(out, "«failed»"),
        ValueView::List(_) | ValueView::Attrs(_) => return Ok(false),
    }
    Ok(true)
}

struct BorrowedPrinter<'v> {
    options: PrintOptions,
    ambiguous: bool,
    seen: HashSet<Identity<'v>>,
    attrs: usize,
    items: usize,
}
impl<'v> BorrowedPrinter<'v> {
    fn print<'h>(
        &mut self,
        l: &Language<'_>,
        m: &'v Machine<'h>,
        value: ValueRoot<'_, 'h>,
        cell: Option<CellRoot<'_, 'h>>,
        out: &mut Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        l.legacy().with_stack(|| {
            m.heap.root_registry().with_roots(|r| {
                if depth > l.legacy().config.max_call_depth as usize {
                    return Err(l.legacy().error_at(
                        ErrorKind::StackOverflow,
                        "stack overflow; max-call-depth exceeded",
                        position(l, m, value),
                    ));
                }
                let result: Result<()> = (|| {
                    if scalar(
                        l,
                        m,
                        r,
                        value,
                        out,
                        self.ambiguous,
                        self.options.max_string_length,
                    )? {
                        return Ok(());
                    }
                    match m.heap.view(value) {
                        ValueView::Attrs(attrs) => {
                            let root = attrs.root(r);
                            if self.options.track_repeated
                                && (!self.ambiguous || !attrs.is_empty())
                                && !self.seen.insert(m.heap.bindings_identity(root))
                            {
                                push(out, "«repeated»");
                                return Ok(());
                            }
                            if depth >= self.options.max_depth {
                                push(out, "{ ... }");
                                return Ok(());
                            }
                            let mut sorted =
                                bindings::lexicographic(&m.heap, r, root, l.legacy().keys);
                            if self.options.max_attrs != usize::MAX {
                                sorted.sort_by_key(|a| {
                                    !matches!(l.legacy().symbol_name(a.name), b"type" | b"_type")
                                });
                            }
                            out.push(b'{');
                            for (index, attr) in sorted.iter().enumerate() {
                                out.push(b' ');
                                if self.attrs >= self.options.max_attrs {
                                    let count = sorted.len() - index;
                                    push(
                                        out,
                                        &format!(
                                            "«{count} {} elided»",
                                            if count == 1 {
                                                "attribute"
                                            } else {
                                                "attributes"
                                            }
                                        ),
                                    );
                                    break;
                                }
                                let name = l.legacy().symbol_name(attr.name);
                                if self.ambiguous {
                                    print_identifier(out, name);
                                } else {
                                    print_attribute_name(out, name);
                                }
                                push(out, " = ");
                                self.print(
                                    l,
                                    m,
                                    m.heap.value(r, attr.value),
                                    Some(attr.value),
                                    out,
                                    depth + 1,
                                )?;
                                out.push(b';');
                                self.attrs += 1;
                            }
                            push(out, " }");
                        }
                        ValueView::List(list) => {
                            if self.options.track_repeated
                                && !list.is_empty()
                                && cell.is_some_and(|cell| {
                                    !self.seen.insert(m.heap.cell_identity(cell))
                                })
                            {
                                push(out, "«repeated»");
                                return Ok(());
                            }
                            if depth >= self.options.max_depth {
                                push(out, "[ ... ]");
                                return Ok(());
                            }
                            out.push(b'[');
                            for index in 0..list.len() {
                                out.push(b' ');
                                if self.items >= self.options.max_list_items {
                                    let count = list.len() - index;
                                    push(
                                        out,
                                        &format!(
                                            "«{count} {} elided»",
                                            if count == 1 { "item" } else { "items" }
                                        ),
                                    );
                                    break;
                                }
                                let child = list.get(index).unwrap().root(r);
                                self.print(
                                    l,
                                    m,
                                    m.heap.value(r, child),
                                    Some(child),
                                    out,
                                    depth + 1,
                                )?;
                                self.items += 1;
                            }
                            push(out, " ]");
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                })();
                match result {
                    Err(error)
                        if self.options.print_errors && error.kind != ErrorKind::StackOverflow =>
                    {
                        push(out, "«error: ");
                        out.extend_from_slice(&error.msg);
                        push(out, "»");
                        Ok(())
                    }
                    other => other,
                }
            })
        })
    }
}

pub(crate) fn print_for_error<'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
) -> Vec<u8> {
    // A spark's failures are discarded and retried in order: do not render
    // a message nobody reads.
    #[cfg(brix_parallel)]
    if m.background.is_some() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut printer = BorrowedPrinter {
        options: PrintOptions::for_errors(),
        ambiguous: false,
        seen: HashSet::new(),
        attrs: 0,
        items: 0,
    };
    if let Err(error) = printer.print(l, m, value, None, &mut out, 0) {
        debug_assert_eq!(error.kind, ErrorKind::StackOverflow);
    }
    out
}

pub(crate) fn print_ambiguous<'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    cell: CellRoot<'_, 'h>,
) -> Result<BString> {
    m.heap.root_registry().with_roots(|r| {
        let mut out = Vec::new();
        let mut printer = BorrowedPrinter {
            options: PrintOptions {
                print_errors: false,
                ..Default::default()
            },
            ambiguous: true,
            seen: HashSet::new(),
            attrs: 0,
            items: 0,
        };
        printer.print(l, m, m.heap.value(r, cell), Some(cell), &mut out, 0)?;
        Ok(out.into())
    })
}

struct Printer<'h> {
    options: PrintOptions,
    seen: Option<Visited<'h>>,
    attrs: usize,
    items: usize,
    indent: Vec<u8>,
}
impl<'h> Printer<'h> {
    fn space(&self, out: &mut Vec<u8>, pretty: bool) {
        if pretty {
            out.push(b'\n');
            out.extend_from_slice(&self.indent);
        } else {
            out.push(b' ');
        }
    }
    fn print(
        &mut self,
        l: &Language<'_>,
        m: &mut Machine<'h>,
        cell: CellRoot<'_, 'h>,
        out: &mut Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        l.legacy().with_stack(|| {
            m.with_roots(|m, r| {
                let value = m.heap.value(r, cell);
                let pos = position(l, m, value);
                if depth > l.legacy().config.max_call_depth as usize {
                    return Err(l.legacy().error_at(
                        ErrorKind::StackOverflow,
                        "stack overflow; max-call-depth exceeded",
                        pos,
                    ));
                }
                let result: Result<()> = (|| {
                    let value = if self.options.force {
                        l.force(m, r, cell, pos)?
                    } else {
                        value
                    };
                    if scalar(l, m, r, value, out, false, self.options.max_string_length)? {
                        return Ok(());
                    }
                    match m.heap.view(value) {
                        ValueView::Attrs(attrs) => {
                            let attrs = attrs.root(r);
                            self.print_attrs(l, m, r, attrs, out, depth)?;
                        }
                        ValueView::List(_) => self.print_list(l, m, r, cell, value, out, depth)?,
                        _ => unreachable!(),
                    }
                    Ok(())
                })();
                match result {
                    Err(error)
                        if self.options.print_errors && error.kind != ErrorKind::StackOverflow =>
                    {
                        push(out, "«error: ");
                        out.extend_from_slice(&error.msg);
                        push(out, "»");
                        Ok(())
                    }
                    other => other,
                }
            })
        })
    }
    fn pretty(
        &self,
        l: &Language<'_>,
        m: &mut Machine<'h>,
        items: &[CellRoot<'_, 'h>],
    ) -> Result<bool> {
        if self.options.pretty_indent == 0 || items.is_empty() {
            return Ok(false);
        }
        if items.len() > 1 {
            return Ok(true);
        }
        m.with_roots(|m, r| {
            let before = m.heap.value(r, items[0]);
            let pos = position(l, m, before);
            let value = l.force(m, r, items[0], pos)?;
            Ok(matches!(
                m.heap.view(value),
                ValueView::List(_) | ValueView::Attrs(_)
            ))
        })
    }
    fn print_attrs(
        &mut self,
        l: &Language<'_>,
        m: &mut Machine<'h>,
        r: &Roots<'_, 'h>,
        attrs: BindingsRoot<'_, 'h>,
        out: &mut Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        if self
            .seen
            .as_ref()
            .is_some_and(|seen| !m.heap.mark_bindings_visited(seen, attrs))
        {
            push(out, "«repeated»");
            return Ok(());
        }
        if self.options.force && self.options.derivation_paths {
            let derivation = m.with_roots(|m, t| -> Result<bool> {
                let Some(attr) = m
                    .heap
                    .bindings_view(attrs)
                    .get(sym::TYPE)
                    .map(|a| a.root(t))
                else {
                    return Ok(false);
                };
                let value = l.force(m, t, attr.value, attr.pos)?;
                match m.heap.view(value) {
                    ValueView::String(_) => Ok(text(l, m, value)? == b"derivation"),
                    _ => Ok(false),
                }
            })?;
            if derivation {
                let path = m.with_roots(|m, t| {
                    let Some(attr) = m.heap.bindings_view(attrs).get(sym::DRV_PATH).map(|a| a.root(t)) else { return Ok(None); };
                    let ctx = "while evaluating the drvPath of a derivation";
                    let value = coerce(l, m, t, attr.value, attr.pos, false, false).map_err(|e| l.legacy().traced(e, attr.pos, ctx))?;
                    let bytes = text(l, m, value)?;
                    let path = l.legacy().config.store_dir.maybe_parse(&bytes).ok_or_else(|| l.legacy().error(ErrorKind::Eval, msg!("cannot coerce '", bytes, "' to a store path because it is not a subpath of the Nix store")).with_trace(l.legacy().position(attr.pos), ctx))?;
                    Ok::<_, crate::Error>(Some(path))
                })?;
                push(out, "«derivation");
                if let Some(path) = path {
                    out.push(b' ');
                    push(out, &l.legacy().config.store_dir.display(&path));
                }
                push(out, "»");
                return Ok(());
            }
        }
        if depth >= self.options.max_depth {
            push(out, "{ ... }");
            return Ok(());
        }
        self.indent
            .resize(self.indent.len() + self.options.pretty_indent, b' ');
        out.push(b'{');
        let mut sorted = bindings::lexicographic(&m.heap, r, attrs, l.legacy().keys);
        if self.options.max_attrs != usize::MAX {
            sorted.sort_by_key(|a| !matches!(l.legacy().symbol_name(a.name), b"type" | b"_type"));
        }
        let items: Vec<_> = sorted.iter().map(|a| a.value).collect();
        let pretty = self.pretty(l, m, &items)?;
        for (index, attr) in sorted.iter().enumerate() {
            self.space(out, pretty);
            if self.attrs >= self.options.max_attrs {
                let count = sorted.len() - index;
                push(
                    out,
                    &format!(
                        "«{count} {} elided»",
                        if count == 1 {
                            "attribute"
                        } else {
                            "attributes"
                        }
                    ),
                );
                break;
            }
            print_attribute_name(out, l.legacy().symbol_name(attr.name));
            push(out, " = ");
            self.print(l, m, attr.value, out, depth + 1)?;
            out.push(b';');
            self.attrs += 1;
        }
        self.indent
            .truncate(self.indent.len() - self.options.pretty_indent);
        self.space(out, pretty);
        out.push(b'}');
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn print_list(
        &mut self,
        l: &Language<'_>,
        m: &mut Machine<'h>,
        r: &Roots<'_, 'h>,
        cell: CellRoot<'_, 'h>,
        value: ValueRoot<'_, 'h>,
        out: &mut Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        let ValueView::List(list) = m.heap.view(value) else {
            unreachable!()
        };
        let len = list.len();
        if len > 0
            && self
                .seen
                .as_ref()
                .is_some_and(|seen| !m.heap.mark_visited(seen, cell))
        {
            push(out, "«repeated»");
            return Ok(());
        }
        if depth >= self.options.max_depth {
            push(out, "[ ... ]");
            return Ok(());
        }
        self.indent
            .resize(self.indent.len() + self.options.pretty_indent, b' ');
        out.push(b'[');
        let sample: Vec<_> = (0..len.min(2))
            .map(|i| list.get(i).unwrap().root(r))
            .collect();
        let pretty = self.pretty(l, m, &sample)?;
        for index in 0..len {
            self.space(out, pretty);
            if self.items >= self.options.max_list_items {
                let count = len - index;
                push(
                    out,
                    &format!(
                        "«{count} {} elided»",
                        if count == 1 { "item" } else { "items" }
                    ),
                );
                break;
            }
            m.with_roots(|m, t| {
                let ValueView::List(list) = m.heap.view(value) else {
                    unreachable!()
                };
                self.print(l, m, list.get(index).unwrap().root(t), out, depth + 1)
            })?;
            self.items += 1;
        }
        self.indent
            .truncate(self.indent.len() - self.options.pretty_indent);
        self.space(out, pretty);
        out.push(b']');
        Ok(())
    }
}

pub(crate) fn print_value<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    _r: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    options: &PrintOptions,
) -> Result<BString> {
    let seen = options.track_repeated.then(|| m.heap.visited());
    let mut printer = Printer {
        options: options.clone(),
        seen,
        attrs: 0,
        items: 0,
        indent: Vec::new(),
    };
    let mut out = Vec::new();
    printer.print(l, m, cell, &mut out, 0)?;
    Ok(out.into())
}

pub(crate) fn to_owned_value<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
) -> Result<OwnedValue> {
    crate::managed_builtins::deep_force(l, m, r, cell, SrcPos::NONE)?;
    fn copy<'v, 'h>(
        l: &Language<'_>,
        m: &'v Machine<'h>,
        cell: CellRoot<'_, 'h>,
        active: &mut HashSet<Identity<'v>>,
        depth: usize,
    ) -> Result<OwnedValue> {
        l.legacy().with_stack(|| {
            m.heap.root_registry().with_roots(|r| {
                let value = m.heap.value(r, cell);
                if depth > l.legacy().config.max_call_depth as usize {
                    return Err(l.legacy().error_at(
                        ErrorKind::StackOverflow,
                        "stack overflow; max-call-depth exceeded",
                        position(l, m, value),
                    ));
                }
                let identity = match m.heap.view(value) {
                    ValueView::Attrs(attrs) => Some(m.heap.bindings_identity(attrs.root(r))),
                    ValueView::List(list) if !list.is_empty() => Some(m.heap.cell_identity(cell)),
                    _ => None,
                };
                if identity.is_some_and(|id| !active.insert(id)) {
                    return Err(l.legacy().error(
                        ErrorKind::InfiniteRecursion,
                        "cannot convert a cyclic value to an owned value",
                    ));
                }
                let result: Result<OwnedValue> = (|| {
                    Ok(match m.heap.view(value) {
                        ValueView::Int(n) => OwnedValue::Int(n),
                        ValueView::Float(n) => OwnedValue::Float(n),
                        ValueView::Bool(b) => OwnedValue::Bool(b),
                        ValueView::Null => OwnedValue::Null,
                        ValueView::String(_) => {
                            let bytes = text(l, m, value)?;
                            let (_, mut context) = strings::snapshot(m, value);
                            context.retain(|c| !matches!(c, OwnedContext::LazyHash { .. }));
                            strings::resolve_context(l, &mut context)?;
                            let context = strings::borrowed_context(&context)?
                                .iter()
                                .map(|c| c.encode())
                                .collect();
                            OwnedValue::String { bytes, context }
                        }
                        ValueView::Path(path) => OwnedValue::Path(
                            l.legacy()
                                .path_text(&PathVal {
                                    accessor: path.accessor(),
                                    path: path.bytes(),
                                })?
                                .into_owned()
                                .into(),
                        ),
                        ValueView::Attrs(attrs) => {
                            let mut result = BTreeMap::new();
                            for attr in attrs.iter() {
                                result.insert(
                                    l.legacy().symbol_name(attr.name).to_vec(),
                                    copy(l, m, attr.value.root(r), active, depth + 1)?,
                                );
                            }
                            OwnedValue::Attrs(result)
                        }
                        ValueView::List(list) => {
                            let mut result = Vec::with_capacity(list.len());
                            for i in 0..list.len() {
                                result.push(copy(
                                    l,
                                    m,
                                    list.get(i).unwrap().root(r),
                                    active,
                                    depth + 1,
                                )?);
                            }
                            OwnedValue::List(result)
                        }
                        _ => OwnedValue::Function(print_for_error(l, m, value)),
                    })
                })();
                if let Some(identity) = identity {
                    active.remove(&identity);
                }
                result
            })
        })
    }
    copy(l, m, cell, &mut HashSet::new(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_eval::allocation;
    use crate::managed_heap::with_owned_heap;

    #[test]
    fn printers_match_legacy_for_laziness_functions_limits_sharing_and_errors() {
        let sources = [
            "[ 1 1.5 true null \"bytes\\n\\\"\" /some/path ]",
            "[ (1 + 2) (throw \"bad\") ]",
            "let f = x: x; in [ f builtins.add (builtins.add 1) ]",
            "let a = { z = 2; a = 1; }; in [ a a {} {} ]",
            "let a = [ 1 2 ]; in [ a a [] [] ]",
            "let a = rec { x = a; }; in a",
            "let a = [ a ]; in a",
            "{ z = [ [1 2] [3 4] ]; a = { b = 1; c = 2; }; type = \"test\"; _type = \"tag\"; }",
            "{ type = \"derivation\"; drvPath = \"/nix/store/00000000000000000000000000000000-test.drv\"; }",
            "{ type = \"derivation\"; drvPath = \"bad\"; }",
            "{ \"a b\" = 1; or = 2; }",
        ];
        let mut mismatches = Vec::new();
        for stress in [false, true] {
            for source in sources {
                let old_arena = crate::Arena::new();
                let new_arena = crate::Arena::new();
                let host = crate::NullHost;
                let old = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
                let old_cell = old.alloc_cell(old.eval_str(source.as_bytes(), "/").unwrap());
                let language = Language::new(crate::Evaluator::new(
                    &new_arena,
                    crate::Config::default(),
                    &host,
                ));
                with_owned_heap(
                    brix_gc::Config { workers: 0,
                        chunk_bytes: 256,
                        steady_threshold_bytes: None, trace_collections: false, threshold_bytes: 1024,
                        stress,
                    },
                    |heap| {
                        let mut machine = Machine::new(heap);
                        machine.with_roots(|machine, roots| {
                            let value = language
                                .eval_str(machine, roots, source.as_bytes(), "/")
                                .unwrap();
                            let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                            let actual = print_for_error(&language, machine, value);
                            let expected = old.print_for_error(old_cell.get()).to_vec();
                            if actual != expected { mismatches.push(format!("error {source}: managed {:?}; legacy {:?}", String::from_utf8_lossy(&actual), String::from_utf8_lossy(&expected))); }
                            let actual = print_ambiguous(&language, machine, cell).unwrap();
                            let expected = old.print_ambiguous(old_cell).unwrap();
                            if actual != expected { mismatches.push(format!("ambiguous {source}: managed {actual:?}; legacy {expected:?}")); }
                            for options in [
                                PrintOptions::for_eval(),
                                PrintOptions {
                                    force: true,
                                    pretty_indent: 2,
                                    ..Default::default()
                                },
                                PrintOptions {
                                    force: true,
                                    max_depth: 2,
                                    max_attrs: 3,
                                    max_list_items: 3,
                                    max_string_length: 4,
                                    ..Default::default()
                                },
                            ] {
                                let actual = print_value(&language, machine, roots, cell, &options).unwrap();
                                let expected = old.print_value(old_cell, &options).unwrap();
                                if actual != expected {
                                    mismatches.push(format!("{source}, {options:?}: managed {actual:?}; legacy {expected:?}"));
                                }
                                machine.heap.collect().unwrap();
                            }
                        });
                    },
                )
                .unwrap();
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    #[test]
    fn owned_snapshots_match_legacy_and_reject_cycles_without_losing_dags() {
        for source in [
            "{ z = [ 1 true null ]; a = \"hello\"; f = x: x; }",
            "let a = { x = [1 2]; }; in [ a a ]",
            "builtins.appendContext \"x\" { \"/nix/store/00000000000000000000000000000000-test\" = { path = true; }; }",
        ] {
            let old_arena = crate::Arena::new();
            let new_arena = crate::Arena::new();
            let host = crate::NullHost;
            let old = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
            let expected = old.to_owned_value(old.alloc_cell(old.eval_str(source.as_bytes(), "/").unwrap())).unwrap();
            let language = Language::new(crate::Evaluator::new(&new_arena, crate::Config::default(), &host));
            with_owned_heap(brix_gc::Config { workers: 0, chunk_bytes: 256, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: 1024, stress: true }, |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let value = language.eval_str(machine, roots, source.as_bytes(), "/").unwrap();
                    let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                    assert_eq!(to_owned_value(&language, machine, roots, cell).unwrap(), expected);
                    let cyclic = language.eval_str(machine, roots, b"let x = { next = x; }; in x", "/").unwrap();
                    let cyclic = allocation(machine.heap.cell(roots, cyclic)).unwrap();
                    let error = to_owned_value(&language, machine, roots, cyclic).expect_err("cycles cannot be copied into OwnedValue");
                    assert_eq!(error.kind, ErrorKind::InfiniteRecursion);
                    machine.heap.collect().unwrap();
                    assert_eq!(to_owned_value(&language, machine, roots, cell).unwrap(), expected);
                });
            }).unwrap();
        }
    }
    #[test]
    fn printers_handle_failed_lazy_hashes_and_opaque_context_without_collecting() {
        struct Broken;
        impl brix_store::SourceAccessor for Broken {
            fn maybe_lstat(
                &self,
                _: &brix_store::CanonPath,
            ) -> std::result::Result<Option<brix_store::Stat>, brix_store::AccessorError>
            {
                Err(brix_store::AccessorError::Io("lazy hash failed".into()))
            }
            fn read_dir(
                &self,
                _: &brix_store::CanonPath,
            ) -> std::result::Result<brix_store::DirEntries, brix_store::AccessorError>
            {
                unreachable!()
            }
            fn read_file(
                &self,
                _: &brix_store::CanonPath,
            ) -> std::result::Result<Vec<u8>, brix_store::AccessorError> {
                unreachable!()
            }
            fn read_link(
                &self,
                _: &brix_store::CanonPath,
            ) -> std::result::Result<BString, brix_store::AccessorError> {
                unreachable!()
            }
        }
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let ev = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let (_, path) = ev.mount_lazy_tree(std::sync::Arc::new(Broken), "source", "/broken");
        let language = Language::new(ev);
        with_owned_heap(
            brix_gc::Config {
                workers: 0,
                chunk_bytes: 256,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 1024,
                stress: true,
            },
            |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|machine, roots| {
                    let value = crate::managed_builtins_paths::lazy_path_string(
                        &language,
                        machine,
                        roots,
                        path.as_bytes(),
                        true,
                    )
                    .unwrap();
                    let cell = allocation(machine.heap.cell(roots, value)).unwrap();
                    let before = machine.heap.stats().collections;
                    assert_eq!(
                        print_for_error(&language, machine, value),
                        "«error: lazy hash failed»".as_bytes()
                    );
                    assert_eq!(machine.heap.stats().collections, before);
                    assert!(print_ambiguous(&language, machine, cell).is_err());
                    let options = PrintOptions {
                        force: true,
                        print_errors: false,
                        ..Default::default()
                    };
                    assert!(print_value(&language, machine, roots, cell, &options).is_err());
                    let invalid = strings::string(
                        machine,
                        roots,
                        b"text",
                        &[OwnedContext::Opaque(vec![255])],
                    )
                    .unwrap();
                    let invalid_cell = allocation(machine.heap.cell(roots, invalid)).unwrap();
                    assert_eq!(print_for_error(&language, machine, invalid), b"\"text\"");
                    assert!(to_owned_value(&language, machine, roots, invalid_cell).is_err());
                });
            },
        )
        .unwrap();
    }
}
