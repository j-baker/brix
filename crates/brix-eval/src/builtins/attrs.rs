//! Attribute-set built-ins.

use crate::builtins::{reg, require_attr};
use crate::evaluator::Evaluator;
use crate::symbol::{sym, Symbol};
use crate::value::{Attr, Bindings, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "__attrNames", 1, prim_attr_names);
    reg(ev, "__attrValues", 1, prim_attr_values);
    reg(ev, "__getAttr", 2, prim_get_attr);
    reg(ev, "__unsafeGetAttrPos", 2, prim_unsafe_get_attr_pos);
    reg(ev, "__hasAttr", 2, prim_has_attr);
    reg(ev, "__isAttrs", 1, prim_is_attrs);
    reg(ev, "removeAttrs", 2, prim_remove_attrs);
    reg(ev, "__listToAttrs", 1, prim_list_to_attrs);
    reg(ev, "__intersectAttrs", 2, prim_intersect_attrs);
    reg(ev, "__catAttrs", 2, prim_cat_attrs);
    reg(ev, "__mapAttrs", 2, prim_map_attrs);
    reg(ev, "__filterAttrs", 2, prim_filter_attrs);
    reg(ev, "__zipAttrsWith", 2, prim_zip_attrs_with);
}

fn prim_attr_names<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let b = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrNames",
    )?;
    // Sorted by name, as Nix sorts the list it builds.
    let sorted = b.lexicographic(ev.keys);
    Ok(ev.mk_list_from_iter(sorted.len(), sorted.iter().map(|a| ev.symbol_value(a.name))))
}

fn prim_attr_values<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let b = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrValues",
    )?;
    // In `attrNames` order: Nix sorts the attributes by name.
    let sorted = b.lexicographic(ev.keys);
    Ok(ev.mk_list_from_iter(sorted.len(), sorted.iter().map(|a| a.value)))
}

fn prim_get_attr<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getAttr",
    )?;
    let b = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.getAttr",
    )?;
    let sym = ev.intern(name);
    let attr = require_attr(ev, b, sym, "in the attribute set under consideration")?;
    ev.force(attr.value, pos)
}

fn prim_unsafe_get_attr_pos<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.unsafeGetAttrPos",
    )?;
    let b = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.unsafeGetAttrPos",
    )?;
    match b.get(ev.intern(name)) {
        None => Ok(Value::Null),
        Some(a) => ev.mk_pos_value(a.pos),
    }
}

fn prim_has_attr<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hasAttr",
    )?;
    let b = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hasAttr",
    )?;
    Ok(Value::Bool(b.get(ev.intern(name)).is_some()))
}

fn prim_is_attrs<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    Ok(Value::Bool(matches!(v, Value::Attrs(_))))
}

fn prim_remove_attrs<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let b = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.removeAttrs",
    )?;
    let names = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.removeAttrs",
    )?;
    let mut remove: Vec<Symbol> = Vec::with_capacity(names.len());
    for &n in names {
        let s = ev.force_string_no_ctx(
            n,
            pos,
            "while evaluating the values of the second argument passed to builtins.removeAttrs",
        )?;
        remove.push(ev.intern(s));
    }
    // Nix builds a new set even when nothing is removed (identity is not
    // observable here). nixpkgs calls this on large sets constantly; a
    // large set is a tree, from which `Bindings::remove` drops the names
    // by path copying.
    Ok(ev.mk_bindings(Bindings::remove(ev.bump, b, &remove)))
}

fn prim_list_to_attrs<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.listToAttrs",
    )?;
    // As Nix: first every element and its `name` are forced, in list order;
    // then the pairs are sorted by name (symbol id, then list position),
    // the first pair of each name wins, and only the winners' `value`
    // attributes are looked up, in that order. So a `name` that throws
    // beats an earlier element's missing `value`, and a shadowed
    // duplicate needs no `value` at all.
    let mut named: Vec<(Symbol, Bindings<'a>)> = Vec::with_capacity(list.len());
    for &elem in list {
        let b = ev.force_attrs(
            elem,
            pos,
            "while evaluating an element of the list passed to builtins.listToAttrs",
        )?;
        let name_attr = require_attr(ev, b, sym::NAME, "in a {name=...; value=...;} pair")?;
        let name = ev.force_string_no_ctx(
            name_attr.value,
            name_attr.pos,
            "while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs",
        )?;
        named.push((ev.intern(name), b));
    }
    // Stable, so each run of equal names keeps list order and the dedup
    // keeps its first element.
    named.sort_by_key(|&(name, _)| name);
    named.dedup_by_key(|&mut (name, _)| name);
    let mut attrs: Vec<Attr<'a>> = Vec::with_capacity(named.len());
    for (name, b) in named {
        let value_attr = require_attr(ev, b, sym::VALUE, "in a {name=...; value=...;} pair")?;
        attrs.push(Attr::new(name, value_attr.pos, value_attr.value));
    }
    Ok(ev.mk_attrs_sorted(&attrs, SrcPos::NONE))
}

fn prim_intersect_attrs<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let left = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.intersectAttrs",
    )?;
    let right = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.intersectAttrs",
    )?;
    // As Nix: walk the smaller set, look each name up in the larger one.
    // Either walk is in symbol order, so the result is already sorted.
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    if left.len() < right.len() {
        for l in left.iter() {
            if let Some(r) = right.get(l.name) {
                attrs.push(*r);
            }
        }
    } else {
        for r in right.iter() {
            if left.get(r.name).is_some() {
                attrs.push(*r);
            }
        }
    }
    Ok(ev.mk_attrs_sorted(&attrs, SrcPos::NONE))
}

fn prim_cat_attrs<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.catAttrs",
    )?;
    let name = ev.intern(name);
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.catAttrs",
    )?;
    let mut res: Vec<ValueRef<'a>> = Vec::new();
    for &elem in list {
        let b = ev.force_attrs(elem, pos, "while evaluating an element in the list passed as second argument to builtins.catAttrs")?;
        if let Some(a) = b.get(name) {
            res.push(a.value);
        }
    }
    Ok(ev.mk_list(&res))
}

fn prim_map_attrs<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let b = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.mapAttrs",
    )?;
    let mapped = Bindings::map(ev.bump, b, SrcPos::NONE, |a| {
        let f_name = ev.mk_app(args[0], ev.symbol_value(a.name));
        Attr::new(a.name, SrcPos::NONE, ev.mk_app(f_name, a.value))
    });
    Ok(ev.mk_bindings(mapped))
}

fn prim_filter_attrs<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let b = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.filterAttrs",
    )?;
    if b.is_empty() {
        return Ok(args[1].get());
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filterAttrs",
    )?;
    // The predicate is called in `Bindings` (symbol id) order, as in Nix,
    // which is observable through `trace` and through which call fails
    // first. The kept attributes stay in that order.
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    for a in b.iter() {
        let r = ev.call(f, &[ev.symbol_value(a.name), a.value], SrcPos::NONE)?;
        let rc = ev.alloc_cell(r);
        if ev.force_bool(rc, pos, "while evaluating the return value of the filtering function passed to builtins.filterAttrs")? {
            attrs.push(Attr::new(a.name, SrcPos::NONE, a.value));
        }
    }
    Ok(ev.mk_attrs_sorted(&attrs, SrcPos::NONE))
}

fn prim_zip_attrs_with<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.zipAttrsWith",
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.zipAttrsWith",
    )?;
    let mut seen: Vec<(Symbol, ValueRef<'a>)> = Vec::new();
    for &elem in list {
        let b = ev.force_attrs(elem, SrcPos::NONE, "while evaluating a value of the list passed as second argument to builtins.zipAttrsWith")?;
        for a in b.iter() {
            seen.push((a.name, a.value));
        }
    }
    // Nix's `sortByName`: a stable sort by symbol, so each name's values
    // stay in list order.
    seen.sort_by_key(|&(name, _)| name);
    let fc = ev.alloc_cell(f);
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    let mut i = 0;
    while i < seen.len() {
        let name = seen[i].0;
        let mut j = i;
        while j < seen.len() && seen[j].0 == name {
            j += 1;
        }
        let values: Vec<ValueRef<'a>> = seen[i..j].iter().map(|(_, v)| *v).collect();
        let call1 = ev.mk_app(fc, ev.symbol_value(name));
        let list_cell = ev.alloc_cell(ev.mk_list(&values));
        attrs.push(Attr::new(name, SrcPos::NONE, ev.mk_app(call1, list_cell)));
        i = j;
    }
    Ok(ev.mk_attrs_sorted(&attrs, SrcPos::NONE))
}
