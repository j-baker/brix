//! String-context built-ins (`primops/context.cc`).

use std::collections::BTreeMap;

use brix_store::StorePath;

use crate::builtins::reg;
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{Attr, ContextElem, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(
        ev,
        "__unsafeDiscardStringContext",
        1,
        prim_unsafe_discard_string_context,
    );
    reg(ev, "__hasContext", 1, prim_has_context);
    reg(
        ev,
        "__unsafeDiscardOutputDependency",
        1,
        prim_unsafe_discard_output_dependency,
    );
    reg(
        ev,
        "__addDrvOutputDependencies",
        1,
        prim_add_drv_output_dependencies,
    );
    reg(ev, "__getContext", 1, prim_get_context);
    reg(ev, "__appendContext", 2, prim_append_context);
}

fn prim_unsafe_discard_string_context<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    // A lazy hash is not context but part of the text, so it stays, and
    // so does the `Path` element of a lazy tree (as `Path` elements do).
    if let Value::String(s) = ev.force_for_coercion(args[0], pos)? {
        if s.is_lazy() {
            let kept: Vec<ContextElem<'a>> = s
                .raw_context()
                .iter()
                .copied()
                .filter(|c| c.is_lazy() || matches!(c, ContextElem::Path(_)))
                .collect();
            return Ok(ev.string_from_arena(s.raw_bytes(), &kept));
        }
    }
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the argument passed to builtins.unsafeDiscardStringContext",
        Coercion::COPY,
    )?;
    let filtered: StringContext<'a> = ctx
        .into_iter()
        .filter(|c| matches!(c, ContextElem::Path(_) | ContextElem::LazyPath(_)))
        .collect();
    Ok(ev.mk_string_arena(s, &filtered))
}

fn prim_has_context<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    // Only the context shown counts, so a lazy hash need not be computed.
    let s = ev.force_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.hasContext",
    )?;
    Ok(Value::Bool(s.has_context()))
}

fn prim_unsafe_discard_output_dependency<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the argument passed to builtins.unsafeDiscardOutputDependency",
        Coercion::COPY,
    )?;
    let out: StringContext<'a> = ctx
        .into_iter()
        .map(|c| match c {
            ContextElem::DrvDeep(p) => ContextElem::Opaque(p),
            other => other,
        })
        .collect();
    Ok(ev.mk_string_arena(s, &out))
}

fn prim_add_drv_output_dependencies<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the argument passed to builtins.addDrvOutputDependencies",
        Coercion::COPY,
    )?;
    if ctx.len() != 1 {
        return Err(ev.error_at(
            ErrorKind::Eval,
            msg!(
                "context of string '",
                s,
                "' must have exactly one element, but has ",
                ctx.len().to_string()
            ),
            pos,
        ));
    }
    let elem = *ctx.iter().next().unwrap();
    let new = match elem {
        ContextElem::Opaque(p) => {
            if !p.ends_with(".drv") {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    format!("path '{}/{p}' is not a derivation", ev.config.store_dir.as_str()),
                    pos,
                ));
            }
            ContextElem::DrvDeep(p)
        }
        ContextElem::Built { output, .. } => {
            return Err(ev.error_at(
                ErrorKind::Eval,
                msg!(
                    "`addDrvOutputDependencies` can only act on derivations, not on a derivation output such as '",
                    output,
                    "'"
                ),
                pos,
            ))
        }
        ContextElem::DrvDeep(p) => ContextElem::DrvDeep(p),
        ContextElem::Path(_) | ContextElem::LazyPath(_) => {
            return Err(ev.error_at(
                ErrorKind::Eval,
                "`addDrvOutputDependencies` does not work on a string without context",
                pos,
            ))
        }
        ContextElem::LazyHash { .. } => {
            unreachable!("an accumulated context never holds lazy hashes")
        }
    };
    let mut out = StringContext::new();
    out.insert(new);
    Ok(ev.mk_string_arena(s, &out))
}

#[derive(Default)]
struct ContextInfo<'a> {
    path: bool,
    all_outputs: bool,
    /// Output names, bytes as they are in the context.
    outputs: Vec<&'a [u8]>,
}

fn prim_get_context<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    ev.force_string_ctx(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the argument passed to builtins.getContext",
    )?;
    let mut infos: BTreeMap<StorePath, ContextInfo<'a>> = BTreeMap::new();
    for c in ctx {
        match c {
            ContextElem::DrvDeep(d) => {
                infos.entry(ev.store_path_of(d)?).or_default().all_outputs = true;
            }
            ContextElem::Built { drv, output } => {
                infos
                    .entry(ev.store_path_of(drv)?)
                    .or_default()
                    .outputs
                    .push(output);
            }
            ContextElem::Opaque(p) => {
                infos.entry(ev.store_path_of(p)?).or_default().path = true;
            }
            // `Path` elements are not shown, a lazy tree's neither.
            ContextElem::Path(_) | ContextElem::LazyPath(_) => {}
            ContextElem::LazyHash { .. } => {
                unreachable!("an accumulated context never holds lazy hashes")
            }
        }
    }
    let s_path = ev.sym("path");
    let s_all = ev.sym("allOutputs");
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    for (sp, info) in infos {
        let mut inner: Vec<Attr<'a>> = Vec::new();
        if info.path {
            inner.push(ev.attr_cell(s_path, ev.consts.true_));
        }
        if info.all_outputs {
            inner.push(ev.attr_cell(s_all, ev.consts.true_));
        }
        if !info.outputs.is_empty() {
            // The names are arena bytes already: the strings share them.
            let list =
                ev.mk_list_of_values(info.outputs.iter().map(|&o| ev.mk_string_view(o, &[])));
            inner.push(ev.attr(sym::OUTPUTS, list));
        }
        let inner_v = ev.mk_attrs(inner, SrcPos::NONE);
        attrs.push(ev.attr(ev.sym(&ev.config.store_dir.display(&sp)), inner_v));
    }
    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
}

fn prim_append_context<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let orig = ev.force_string_ctx(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument passed to builtins.appendContext",
    )?;
    let attrs = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.appendContext",
    )?;
    let s_path = ev.sym("path");
    let s_all = ev.sym("allOutputs");
    for a in attrs.iter() {
        // A name that is not UTF-8 is no store path either.
        let name = ev.symbol_name(a.name);
        let parsed = std::str::from_utf8(name)
            .ok()
            .and_then(|name| ev.config.store_dir.maybe_parse(name));
        let Some(sp) = parsed else {
            return Err(ev.error_at(
                ErrorKind::Eval,
                msg!(
                    "context key '",
                    ev.symbol_ident(a.name),
                    "' is not a store path"
                ),
                a.pos,
            ));
        };
        let base = ev.arena_str(sp.as_str());
        let info = ev.force_attrs(
            a.value,
            a.pos,
            "while evaluating the value of a string context",
        )?;
        if let Some(p) = info.get(s_path) {
            if ev.force_bool(
                p.value,
                p.pos,
                "while evaluating the `path` attribute of a string context",
            )? {
                ctx.insert(ContextElem::Opaque(base));
            }
        }
        if let Some(all) = info.get(s_all) {
            if ev.force_bool(
                all.value,
                all.pos,
                "while evaluating the `allOutputs` attribute of a string context",
            )? {
                if !sp.is_derivation() {
                    return Err(ev.error_at(
                        ErrorKind::Eval,
                        msg!(
                            "tried to add all-outputs context of ",
                            ev.symbol_ident(a.name),
                            ", which is not a derivation, to a string"
                        ),
                        a.pos,
                    ));
                }
                ctx.insert(ContextElem::DrvDeep(base));
            }
        }
        if let Some(outs) = info.get(sym::OUTPUTS) {
            let list = ev.force_list_elems(
                outs.value,
                outs.pos,
                "while evaluating the `outputs` attribute of a string context",
            )?;
            if !list.is_empty() && !sp.is_derivation() {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "tried to add derivation output context of ",
                        ev.symbol_ident(a.name),
                        ", which is not a derivation, to a string"
                    ),
                    a.pos,
                ));
            }
            for &elem in list {
                let out = ev.force_string_no_ctx(
                    elem,
                    outs.pos,
                    "while evaluating an output name within a string context",
                )?;
                // Any bytes, as in Nix: the name is the string's, kept in
                // the arena already.
                ctx.insert(ContextElem::Built {
                    drv: base,
                    output: out,
                });
            }
        }
    }
    Ok(ev.mk_string_arena(orig, &ctx))
}
