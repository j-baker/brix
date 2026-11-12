//! Arithmetic, attribute and list builtins on rooted managed values.
//! Evaluation order and laziness follow the corresponding arena builtins.

use crate::error::{msg, ErrorKind};
use crate::managed_bindings as bindings;
use crate::managed_builtins::{
    compare, equal, force_attrs, force_bool, force_float, force_function, force_int, force_list,
    force_string_no_ctx_value,
};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{BindingsRoot, CellRoot, Heap, RootedAttr, Roots, ValueRoot, ValueView};
use crate::managed_language::Language;
use crate::symbol::{sym, Symbol};
use crate::value::SrcPos;
use crate::Result;

// Diagnostic context is constant for each builtin; do not allocate it on the
// successful path (or once per predicate invocation in any/all).
macro_rules! builtin_context {
    ($name:expr, $prefix:literal, $($builtin:literal),+ $(,)?) => {
        match $name {
            $($builtin => concat!($prefix, $builtin),)+
            _ => unreachable!("builtin context dispatch"),
        }
    };
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

// Each permanent handler supplies a constant name to its inlined group body.
// This lets LLVM discard unrelated branches and their stack temporaries; the
// original Primop name remains responsible for callbacks and error traces.
macro_rules! specialized_handlers {
    ($name:expr; $($group:ident => [$($builtin:literal),+ $(,)?]),+ $(,)?) => {
        Some(match $name {
            $($($builtin => |language, machine, roots, _name, args, pos| {
                $group(language, machine, roots, $builtin, args, pos)
            },)+)+
            _ => return None,
        })
    };
}

pub(crate) fn resolve(name: &str) -> Option<super::managed_builtins::Handler> {
    let name = name.strip_prefix("__").unwrap_or(name);
    specialized_handlers!(name;
        math => ["add", "sub", "mul", "div", "bitAnd", "bitOr", "bitXor", "lessThan", "ceil", "floor"],
        attrs => ["attrNames", "attrValues", "getAttr", "unsafeGetAttrPos", "hasAttr", "isAttrs", "removeAttrs", "listToAttrs", "intersectAttrs", "catAttrs", "mapAttrs", "filterAttrs", "zipAttrsWith"],
        lists => ["isList", "elemAt", "head", "tail", "map", "filter", "elem", "concatLists", "length", "foldl'", "any", "all", "genList", "sort", "partition", "groupBy", "concatMap"],
    )
}

#[inline]
fn cell<'r, 'h>(
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    value: ValueRoot<'_, 'h>,
) -> Result<CellRoot<'r, 'h>> {
    allocation(m.heap.cell(r, value))
}
fn list<'r, 'h>(
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    values: &[CellRoot<'_, 'h>],
) -> Result<ValueRoot<'r, 'h>> {
    allocation(m.heap.list(r, values))
}
fn list_len<'h>(h: &Heap<'h>, value: ValueRoot<'_, 'h>) -> usize {
    let ValueView::List(v) = h.view(value) else {
        unreachable!("checked list")
    };
    v.len()
}
fn list_item<'r, 'h>(
    h: &Heap<'h>,
    r: &Roots<'r, 'h>,
    value: ValueRoot<'_, 'h>,
    index: usize,
) -> CellRoot<'r, 'h> {
    let ValueView::List(v) = h.view(value) else {
        unreachable!("checked list")
    };
    v.get(index).expect("in bounds").root(r)
}
fn elements<'r, 'h>(
    h: &Heap<'h>,
    r: &Roots<'r, 'h>,
    value: ValueRoot<'_, 'h>,
) -> Vec<CellRoot<'r, 'h>> {
    let ValueView::List(v) = h.view(value) else {
        unreachable!("checked list")
    };
    (0..v.len()).map(|i| v.get(i).unwrap().root(r)).collect()
}
fn symbol_cell<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    name: Symbol,
) -> Result<CellRoot<'r, 'h>> {
    let index = name.0 as usize;
    if let Some(Some(cached)) = m.symbol_values.get(index) {
        return Ok(r.copy(cached.root()));
    }
    let value = allocation(m.heap.string(r, l.legacy().symbol_name(name)))?;
    let value = cell(m, r, value)?;
    if m.symbol_values.len() <= index {
        m.symbol_values
            .resize_with(l.legacy().keys.len().max(index + 1), || None);
    }
    m.symbol_values[index] = Some(m.heap.persist(value));
    Ok(value)
}
#[inline]
fn app<'r, 'h>(
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    fun: CellRoot<'_, 'h>,
    arg: CellRoot<'_, 'h>,
) -> Result<CellRoot<'r, 'h>> {
    allocation(m.heap.app_cell(r, fun, arg))
}
fn set<'r, 'h>(
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    mut attrs: Vec<RootedAttr<'r, 'h>>,
) -> Result<ValueRoot<'r, 'h>> {
    bindings::sort_dedup(&mut attrs);
    let b = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, SrcPos::NONE))?;
    Ok(m.heap.bindings_value(r, b))
}
fn required<'r, 'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    r: &Roots<'r, 'h>,
    b: BindingsRoot<'_, 'h>,
    name: Symbol,
    ctx: &str,
) -> Result<RootedAttr<'r, 'h>> {
    m.heap
        .bindings_view(b)
        .get(name)
        .map(|a| a.root(r))
        .ok_or_else(|| {
            l.legacy()
                .error(
                    ErrorKind::TypeError,
                    msg!("attribute '", l.legacy().symbol_ident(name), "' missing"),
                )
                .with_trace(None, ctx)
        })
}
fn determine_pos<'h>(
    l: &Language<'_>,
    m: &Machine<'h>,
    value: ValueRoot<'_, 'h>,
    fallback: SrcPos,
) -> SrcPos {
    match m.heap.view(value) {
        ValueView::Attrs(b) => b.pos(),
        ValueView::Lambda { node, .. } => {
            let (code, lambda) = l.legacy().lambda_def(node);
            l.legacy().src_pos(code, lambda.pos)
        }
        _ => fallback,
    }
}

#[inline(always)]
fn math<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    name: &str,
    a: &[CellRoot<'_, 'h>],
    p: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    if name == "lessThan" {
        l.force_cell(m, r, a[0], p)?;
        l.force_cell(m, r, a[1], p)?;
        return Ok(r.bool(compare(l, m, r, a[0], a[1], SrcPos::NONE, "")?));
    }
    if matches!(name, "ceil" | "floor") {
        let before = m.heap.value(r, a[0]);
        let arg_pos = determine_pos(l, m, before, p);
        let f = force_float(
            l,
            m,
            r,
            a[0],
            arg_pos,
            builtin_context!(
                name,
                "while evaluating the first argument passed to builtins.",
                "ceil",
                "floor"
            ),
        )?;
        let rounded = if name == "ceil" { f.ceil() } else { f.floor() };
        let original = match m.heap.read(a[0]) {
            ValueView::Int(i) => Some(i),
            _ => None,
        };
        let min = i64::MIN as f64;
        let result = if rounded >= min && rounded < -min {
            rounded as i64
        } else if let Some(i) = original {
            return Err(l.legacy().error_at(ErrorKind::Eval,format!("Due to a bug (see https://github.com/NixOS/nix/issues/12899) the NixInt argument {i} caused undefined behavior in previous Nix versions.\n\tFuture Nix versions might implement the correct behavior."),p));
        } else {
            return Err(l.legacy().error_at(
                ErrorKind::Eval,
                format!(
                    "NixFloat argument {} is not in the range of NixInt",
                    brix_syntax::float::format_g(f)
                ),
                p,
            ));
        };
        if let Some(i) = original {
            if i != result {
                return Err(l.legacy().error_at(ErrorKind::Eval,format!("Due to a bug (see https://github.com/NixOS/nix/issues/12899) a loss of precision occurred in previous Nix versions because the NixInt argument {i} was rounded to {result}.\n\tFuture Nix versions might implement the correct behavior."),p));
            }
        }
        return Ok(r.int(result));
    }
    if matches!(name, "bitAnd" | "bitOr" | "bitXor") {
        let x = force_int(
            l,
            m,
            r,
            a[0],
            p,
            builtin_context!(
                name,
                "while evaluating the first argument passed to builtins.",
                "bitAnd",
                "bitOr",
                "bitXor"
            ),
        )?;
        let y = force_int(
            l,
            m,
            r,
            a[1],
            p,
            builtin_context!(
                name,
                "while evaluating the second argument passed to builtins.",
                "bitAnd",
                "bitOr",
                "bitXor"
            ),
        )?;
        return Ok(r.int(match name {
            "bitAnd" => x & y,
            "bitOr" => x | y,
            _ => x ^ y,
        }));
    }
    l.force_cell(m, r, a[0], p)?;
    l.force_cell(m, r, a[1], p)?;
    let floats = matches!(m.heap.read(a[0]), ValueView::Float(_))
        || matches!(m.heap.read(a[1]), ValueView::Float(_));
    if name == "div" {
        let divisor = force_float(
            l,
            m,
            r,
            a[1],
            p,
            "while evaluating the second operand of the division",
        )?;
        if divisor == 0.0 {
            return Err(l.legacy().error_at(ErrorKind::Eval, "division by zero", p));
        }
        if floats {
            return Ok(r.float(
                force_float(
                    l,
                    m,
                    r,
                    a[0],
                    p,
                    "while evaluating the first operand of the division",
                )? / divisor,
            ));
        }
        let x = force_int(
            l,
            m,
            r,
            a[0],
            p,
            "while evaluating the first operand of the division",
        )?;
        let y = force_int(
            l,
            m,
            r,
            a[1],
            p,
            "while evaluating the second operand of the division",
        )?;
        return x.checked_div(y).map(|v| r.int(v)).ok_or_else(|| {
            l.legacy().error_at(
                ErrorKind::Eval,
                format!("integer overflow in dividing {x} / {y}"),
                p,
            )
        });
    }
    let (first, second, verb, symbol) = match name {
        "add" => (
            "while evaluating the first argument of the addition",
            "while evaluating the second argument of the addition",
            "adding",
            '+',
        ),
        "sub" => (
            "while evaluating the first argument of the subtraction",
            "while evaluating the second argument of the subtraction",
            "subtracting",
            '-',
        ),
        _ => (
            "while evaluating the first argument of the multiplication",
            "while evaluating the second argument of the multiplication",
            "multiplying",
            '*',
        ),
    };
    if floats {
        let first = if name == "mul" {
            "while evaluating the first of the multiplication"
        } else {
            first
        };
        let x = force_float(l, m, r, a[0], p, first)?;
        let y = force_float(l, m, r, a[1], p, second)?;
        return Ok(r.float(match name {
            "add" => x + y,
            "sub" => x - y,
            _ => x * y,
        }));
    }
    let x = force_int(l, m, r, a[0], p, first)?;
    let y = force_int(l, m, r, a[1], p, second)?;
    (match name {
        "add" => x.checked_add(y),
        "sub" => x.checked_sub(y),
        _ => x.checked_mul(y),
    })
    .map(|v| r.int(v))
    .ok_or_else(|| {
        l.legacy().error_at(
            ErrorKind::Eval,
            format!("integer overflow in {verb} {x} {symbol} {y}"),
            p,
        )
    })
}

#[inline(always)]
fn attrs<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    name: &str,
    a: &[CellRoot<'_, 'h>],
    p: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    match name {
        "isAttrs" => {
            l.force_cell(m, r, a[0], p)?;
            Ok(r.bool(matches!(m.heap.read(a[0]), ValueView::Attrs(_))))
        }
        "attrNames" | "attrValues" => {
            let b = force_attrs(
                l,
                m,
                r,
                a[0],
                p,
                builtin_context!(
                    name,
                    "while evaluating the argument passed to builtins.",
                    "attrNames",
                    "attrValues"
                ),
            )?;
            let values = if name == "attrNames" {
                let names = bindings::lexicographic_names(&m.heap, b, l.legacy().keys);
                let mut values = Vec::with_capacity(names.len());
                for name in names {
                    values
                        .push(m.with_roots(|m, t| symbol_cell(l, m, t, name).map(|c| r.copy(c)))?);
                }
                values
            } else {
                bindings::lexicographic(&m.heap, r, b, l.legacy().keys)
                    .into_iter()
                    .map(|attr| attr.value)
                    .collect()
            };
            list(m, r, &values)
        }
        "getAttr" | "hasAttr" | "unsafeGetAttrPos" => {
            let text = force_string_no_ctx_value(
                l,
                m,
                r,
                a[0],
                p,
                builtin_context!(
                    name,
                    "while evaluating the first argument passed to builtins.",
                    "getAttr",
                    "hasAttr",
                    "unsafeGetAttrPos"
                ),
            )?;
            let b = force_attrs(
                l,
                m,
                r,
                a[1],
                p,
                builtin_context!(
                    name,
                    "while evaluating the second argument passed to builtins.",
                    "getAttr",
                    "hasAttr",
                    "unsafeGetAttrPos"
                ),
            )?;
            let symbol = l
                .legacy()
                .intern(crate::managed_builtins_strings::bytes(m, text));
            if name == "hasAttr" {
                return Ok(r.bool(m.heap.bindings_view(b).get(symbol).is_some()));
            }
            if name == "unsafeGetAttrPos" {
                let pos = m.heap.bindings_view(b).get(symbol).map(|a| a.pos);
                return match pos {
                    Some(pos) => l.position_value(m, r, pos),
                    None => Ok(r.null()),
                };
            }
            let attr = required(
                l,
                m,
                r,
                b,
                symbol,
                "in the attribute set under consideration",
            )?;
            l.force(m, r, attr.value, p)
        }
        "removeAttrs" => {
            let b = force_attrs(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.removeAttrs",
            )?;
            let names = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.removeAttrs",
            )?;
            let mut remove = Vec::with_capacity(list_len(&m.heap, names));
            for index in 0..list_len(&m.heap, names) {
                remove.push(m.with_roots(|m,t|{
                    let cell=list_item(&m.heap,t,names,index);
                    let text=force_string_no_ctx_value(l,m,t,cell,p,"while evaluating the values of the second argument passed to builtins.removeAttrs")?;
                    Ok::<_,crate::Error>(l.legacy().intern(crate::managed_builtins_strings::bytes(m, text)))
                })?);
            }
            let b = allocation(bindings::remove(&mut m.heap, r, b, &remove))?;
            Ok(m.heap.bindings_value(r, b))
        }
        "listToAttrs" => {
            let input = force_list(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the argument passed to builtins.listToAttrs",
            )?;
            let mut named = Vec::with_capacity(list_len(&m.heap, input));
            for index in 0..list_len(&m.heap, input) {
                named.push(m.with_roots(|m,t|{
                    let elem=list_item(&m.heap,t,input,index);
                    let b=force_attrs(l,m,t,elem,p,"while evaluating an element of the list passed to builtins.listToAttrs")?;
                    let attr=required(l,m,t,b,sym::NAME,"in a {name=...; value=...;} pair")?;
                    let text=force_string_no_ctx_value(l,m,t,attr.value,attr.pos,"while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs")?;
                    Ok::<_,crate::Error>((l.legacy().intern(crate::managed_builtins_strings::bytes(m, text)),r.copy(b)))
                })?);
            }
            named.sort_by_key(|&(name, _)| name);
            named.dedup_by_key(|&mut (name, _)| name);
            let mut attrs = Vec::with_capacity(named.len());
            for (name, b) in named {
                let attr = required(l, m, r, b, sym::VALUE, "in a {name=...; value=...;} pair")?;
                attrs.push(RootedAttr { name, ..attr });
            }
            set(m, r, attrs)
        }
        "intersectAttrs" => {
            let left = force_attrs(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.intersectAttrs",
            )?;
            let right = force_attrs(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.intersectAttrs",
            )?;
            let left = m.heap.bindings_view(left);
            let right = m.heap.bindings_view(right);
            let attrs = if left.len() < right.len() {
                left.iter()
                    .filter_map(|a| right.get(a.name))
                    .map(|a| a.root(r))
                    .collect()
            } else {
                right
                    .iter()
                    .filter(|a| left.get(a.name).is_some())
                    .map(|a| a.root(r))
                    .collect()
            };
            set(m, r, attrs)
        }
        "catAttrs" => {
            let text = force_string_no_ctx_value(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.catAttrs",
            )?;
            let name = l
                .legacy()
                .intern(crate::managed_builtins_strings::bytes(m, text));
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.catAttrs",
            )?;
            let mut values = Vec::new();
            for index in 0..list_len(&m.heap, input) {
                if let Some(value)=m.with_roots(|m,t|{
                    let elem=list_item(&m.heap,t,input,index);
                    let b=force_attrs(l,m,t,elem,p,"while evaluating an element in the list passed as second argument to builtins.catAttrs")?;
                    Ok::<_,crate::Error>(m.heap.bindings_view(b).get(name).map(|a|a.value.root(r)))
                })? {values.push(value);}
            }
            list(m, r, &values)
        }
        "mapAttrs" | "filterAttrs" => {
            let b = force_attrs(
                l,
                m,
                r,
                a[1],
                p,
                builtin_context!(
                    name,
                    "while evaluating the second argument passed to builtins.",
                    "mapAttrs",
                    "filterAttrs"
                ),
            )?;
            if name == "mapAttrs" {
                let missing = m.heap.missing_attr_names(b, &m.symbol_values);
                for name in missing {
                    m.with_roots(|m, t| symbol_cell(l, m, t, name).map(|_| ()))?;
                }
                let mapped = allocation(m.heap.map_attrs_lazy(r, b, a[0], &m.symbol_values))?;
                return Ok(m.heap.bindings_value(r, mapped));
            }
            if name == "filterAttrs" && m.heap.bindings_view(b).is_empty() {
                return Ok(m.heap.value(r, a[1]));
            }
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.filterAttrs",
            )?;
            let mut out = bindings::attributes(&m.heap, r, b);
            let mut kept = 0;
            for index in 0..out.len() {
                let attr = out[index];
                let next=m.with_roots(|m,t|{
                    let name_cell=symbol_cell(l,m,t,attr.name)?;
                    let input=t.copy(attr.value);
                    let value=l.call_value_in_frame(m,t,f,&[name_cell,input],SrcPos::NONE)?;
                    let rc=cell(m,t,value)?;
                    if !force_bool(l,m,t,rc,p,"while evaluating the return value of the filtering function passed to builtins.filterAttrs")? {return Ok(None);}
                    let value=attr.value;
                    Ok::<_,crate::Error>(Some(RootedAttr{name:attr.name,pos:SrcPos::NONE,value}))
                })?;
                if let Some(attr) = next {
                    out[kept] = attr;
                    kept += 1;
                }
            }
            out.truncate(kept);
            let b = allocation(bindings::from_sorted(&mut m.heap, r, &out, SrcPos::NONE))?;
            Ok(m.heap.bindings_value(r, b))
        }
        "zipAttrsWith" => {
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.zipAttrsWith",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.zipAttrsWith",
            )?;
            let mut seen = Vec::new();
            for index in 0..list_len(&m.heap, input) {
                m.with_roots(|m,t|{
                let elem=list_item(&m.heap,t,input,index);
                let b=force_attrs(l,m,t,elem,SrcPos::NONE,"while evaluating a value of the list passed as second argument to builtins.zipAttrsWith")?;
                seen.extend(m.heap.bindings_view(b).iter().map(|a|(a.name,a.value.root(r))));
                Ok::<_,crate::Error>(())
            })?;
            }
            seen.sort_by_key(|&(name, _)| name);
            let fc = cell(m, r, f)?;
            let mut attrs = Vec::new();
            let mut i = 0;
            while i < seen.len() {
                let name = seen[i].0;
                let mut j = i + 1;
                while j < seen.len() && seen[j].0 == name {
                    j += 1;
                }
                attrs.push(m.with_roots(|m, t| {
                    let name_cell = symbol_cell(l, m, t, name)?;
                    let first = app(m, t, fc, name_cell)?;
                    let values: Vec<_> = seen[i..j].iter().map(|(_, c)| *c).collect();
                    let values = list(m, t, &values)?;
                    let values = cell(m, t, values)?;
                    let value = app(m, t, first, values)?;
                    Ok::<_, crate::Error>(RootedAttr {
                        name,
                        pos: SrcPos::NONE,
                        value: r.copy(value),
                    })
                })?);
                i = j;
            }
            set(m, r, attrs)
        }
        _ => unreachable!(),
    }
}

#[inline(always)]
fn lists<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    name: &str,
    a: &[CellRoot<'_, 'h>],
    p: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    match name {
        "isList" => {
            l.force_cell(m, r, a[0], p)?;
            Ok(r.bool(matches!(m.heap.read(a[0]), ValueView::List(_))))
        }
        "elemAt" => {
            let n = force_int(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to 'builtins.elemAt'",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to 'builtins.elemAt'",
            )?;
            let len = list_len(&m.heap, input);
            if n < 0 || n as usize >= len {
                return Err(l.legacy().error_at(
                    ErrorKind::Eval,
                    format!("'builtins.elemAt' called with index {n} on a list of size {len}"),
                    p,
                ));
            }
            let elem = list_item(&m.heap, r, input, n as usize);
            l.force(m, r, elem, p)
        }
        "head" | "tail" => {
            let input = force_list(
                l,
                m,
                r,
                a[0],
                p,
                match name {
                    "head" => "while evaluating the first argument passed to 'builtins.head'",
                    _ => "while evaluating the first argument passed to 'builtins.tail'",
                },
            )?;
            let len = list_len(&m.heap, input);
            if len == 0 {
                return Err(l.legacy().error_at(
                    ErrorKind::Eval,
                    format!("'builtins.{name}' called on an empty list"),
                    p,
                ));
            }
            if name == "head" {
                let first = list_item(&m.heap, r, input, 0);
                l.force(m, r, first, p)
            } else {
                let values: Vec<_> = (1..len).map(|i| list_item(&m.heap, r, input, i)).collect();
                list(m, r, &values)
            }
        }
        "length" => {
            let input = force_list(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.length",
            )?;
            Ok(r.int(list_len(&m.heap, input) as i64))
        }
        "map" | "filter" => {
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                builtin_context!(
                    name,
                    "while evaluating the second argument passed to builtins.",
                    "map",
                    "filter"
                ),
            )?;
            let len = list_len(&m.heap, input);
            if len == 0 {
                return Ok(m.heap.value(r, a[1]));
            }
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                builtin_context!(
                    name,
                    "while evaluating the first argument passed to builtins.",
                    "map",
                    "filter"
                ),
            )?;
            let mut values = Vec::with_capacity(len);
            for index in 0..len {
                let next=m.with_roots(|m,t|{
                    let elem=list_item(&m.heap,t,input,index);
                    if name=="map" {return app(m,t,a[0],elem).map(|c|Some(r.copy(c)));}
                    let value=l.call_value_in_frame(m,t,f,&[elem],SrcPos::NONE)?;
                    let rc=cell(m,t,value)?;
                    if force_bool(l,m,t,rc,p,"while evaluating the return value of the filtering function passed to builtins.filter")?{Ok(Some(r.copy(elem)))}else{Ok(None)}
                })?;
                if let Some(value) = next {
                    values.push(value);
                }
            }
            if name == "filter" && values.len() == len {
                Ok(m.heap.value(r, a[1]))
            } else {
                list(m, r, &values)
            }
        }
        "elem" => {
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.elem",
            )?;
            let len = list_len(&m.heap, input);
            let mut index = 0;
            while index < len {
                // One preparation covers every comparison until a fallback.
                // The borrowed scan ends before forcing can mutate or move cells.
                let found = (|| {
                    let ValueView::List(items) = m.heap.view(input) else {
                        unreachable!()
                    };
                    match super::managed_builtins::scan_equal_ready(
                        l,
                        &m.heap,
                        m.heap.cell_view(a[0]),
                        items,
                        index,
                        p,
                    )? {
                        super::managed_builtins::EqualityScan::Found => return Ok(true),
                        super::managed_builtins::EqualityScan::Exhausted => {
                            index = len;
                            return Ok(false);
                        }
                        super::managed_builtins::EqualityScan::Fallback(next) => index = next,
                    }
                    let found = m.with_roots(|m, t| {
                        let elem = list_item(&m.heap, t, input, index);
                        equal(l, m, t, a[0], elem, p)
                    })?;
                    index += 1;
                    Ok(found)
                })()
                .map_err(|e| {
                    l.legacy().traced(
                        e,
                        p,
                        "while searching for the presence of the given element in the list",
                    )
                })?;
                if found {
                    return Ok(r.bool(true));
                }
            }
            Ok(r.bool(false))
        }
        "concatLists" => {
            let input = force_list(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.concatLists",
            )?;
            let last = r.slot(r.null());
            let mut out = Vec::new();
            for index in 0..list_len(&m.heap, input) {
                m.with_roots(|m, t| {
                    let elem = list_item(&m.heap, t, input, index);
                    let sub = force_list(
                        l,
                        m,
                        t,
                        elem,
                        p,
                        "while evaluating a value of the list passed to builtins.concatLists",
                    )?;
                    if list_len(&m.heap, sub) > 0 {
                        last.set(sub);
                    }
                    out.extend(elements(&m.heap, r, sub));
                    Ok::<_, crate::Error>(())
                })?;
            }
            if let ValueView::List(v) = m.heap.view(last.root()) {
                if v.len() == out.len() {
                    return Ok(last.root());
                }
            }
            list(m, r, &out)
        }
        "foldl'" => {
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.foldlStrict",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[2],
                p,
                "while evaluating the third argument passed to builtins.foldlStrict",
            )?;
            if list_len(&m.heap, input) == 0 {
                return l.force(m, r, a[1], p);
            }
            let current = r.slot(a[1]);
            for index in 0..list_len(&m.heap, input) {
                m.with_roots(|m, t| {
                    let elem = list_item(&m.heap, t, input, index);
                    let accumulator = t.copy(current.root());
                    let value = l.call_value_in_frame(m, t, f, &[accumulator, elem], p)?;
                    let next = cell(m, t, value)?;
                    current.set(next);
                    Ok::<_, crate::Error>(())
                })?;
            }
            l.force(m, r, current.root(), p)
        }
        "any" | "all" => {
            let any = name == "any";
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                builtin_context!(
                    name,
                    "while evaluating the first argument passed to builtins.",
                    "any",
                    "all"
                ),
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                builtin_context!(
                    name,
                    "while evaluating the second argument passed to builtins.",
                    "any",
                    "all"
                ),
            )?;
            for index in 0..list_len(&m.heap, input) {
                let result = m.with_roots(|m, t| {
                    let elem = list_item(&m.heap, t, input, index);
                    let value = l.call_value_in_frame(m, t, f, &[elem], p)?;
                    let rc = cell(m, t, value)?;
                    force_bool(
                        l,
                        m,
                        t,
                        rc,
                        p,
                        builtin_context!(
                            name,
                            "while evaluating the return value of the function passed to builtins.",
                            "any",
                            "all"
                        ),
                    )
                })?;
                if result == any {
                    return Ok(r.bool(any));
                }
            }
            Ok(r.bool(!any))
        }
        "genList" => {
            let len = force_int(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.genList",
            )?;
            if len < 0 {
                return Err(l.legacy().error_at(
                    ErrorKind::Eval,
                    format!("cannot create list of size {len}"),
                    p,
                ));
            }
            force_function(
                l,
                m,
                r,
                a[0],
                SrcPos::NONE,
                "while evaluating the first argument passed to builtins.genList",
            )?;
            let mut values = Vec::new();
            if usize::try_from(len)
                .ok()
                .and_then(|n| values.try_reserve_exact(n).ok())
                .is_none()
            {
                return Err(l.legacy().error(ErrorKind::OutOfMemory, "out of memory"));
            }
            for index in 0..len {
                values.push(m.with_roots(|m, t| {
                    let arg = cell(m, t, t.int(index))?;
                    let value = app(m, t, a[0], arg)?;
                    Ok::<_, crate::Error>(r.copy(value))
                })?);
            }
            list(m, r, &values)
        }
        "sort" => {
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.sort",
            )?;
            if list_len(&m.heap, input) == 0 {
                return Ok(m.heap.value(r, a[1]));
            }
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.sort",
            )?;
            let mut values = elements(&m.heap, r, input);
            for &elem in &values {
                l.force_cell(m, r, elem, p)?;
            }
            let native =
                matches!(m.heap.view(f),ValueView::PrimOp(id) if l.primop_name(id)=="lessThan");
            crate::builtins::lists::peeksort(&mut values, &mut |left, right| {
                m.with_roots(|m,t|{
                if native{return compare(l,m,t,left,right,SrcPos::NONE,"while evaluating the ordering function passed to builtins.sort");}
                let value=l.call_value_in_frame(m,t,f,&[left,right],SrcPos::NONE)?;
                let rc=cell(m,t,value)?;
                force_bool(l,m,t,rc,p,"while evaluating the return value of the sorting function passed to builtins.sort")
            })
            })?;
            list(m, r, &values)
        }
        "partition" => {
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.partition",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.partition",
            )?;
            let mut right = Vec::new();
            let mut wrong = Vec::new();
            for index in 0..list_len(&m.heap, input) {
                let (yes,elem)=m.with_roots(|m,t|{
                let elem=list_item(&m.heap,t,input,index);l.force_cell(m,t,elem,p)?;
                let value=l.call_value_in_frame(m,t,f,&[elem],p)?;let rc=cell(m,t,value)?;
                let yes=force_bool(l,m,t,rc,p,"while evaluating the return value of the partition function passed to builtins.partition")?;
                Ok::<_,crate::Error>((yes,r.copy(elem)))
            })?;
                if yes {
                    right.push(elem);
                } else {
                    wrong.push(elem);
                }
            }
            let right = list(m, r, &right)?;
            let right = cell(m, r, right)?;
            let wrong = list(m, r, &wrong)?;
            let wrong = cell(m, r, wrong)?;
            set(
                m,
                r,
                vec![
                    RootedAttr {
                        name: sym::RIGHT,
                        pos: SrcPos::NONE,
                        value: right,
                    },
                    RootedAttr {
                        name: sym::WRONG,
                        pos: SrcPos::NONE,
                        value: wrong,
                    },
                ],
            )
        }
        "groupBy" => {
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.groupBy",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.groupBy",
            )?;
            let mut seen = Vec::with_capacity(list_len(&m.heap, input));
            for index in 0..list_len(&m.heap, input) {
                seen.push(m.with_roots(|m,t|{
                let elem=list_item(&m.heap,t,input,index);let value=l.call_value_in_frame(m,t,f,&[elem],p)?;let rc=cell(m,t,value)?;
                let name=force_string_no_ctx_value(l,m,t,rc,p,"while evaluating the return value of the grouping function passed to builtins.groupBy")?;
                Ok::<_,crate::Error>((l.legacy().intern(crate::managed_builtins_strings::bytes(m, name)),r.copy(elem)))
            })?);
            }
            seen.sort_by_key(|&(name, _)| name);
            let mut attrs = Vec::new();
            let mut i = 0;
            while i < seen.len() {
                let name = seen[i].0;
                let mut j = i + 1;
                while j < seen.len() && seen[j].0 == name {
                    j += 1;
                }
                attrs.push(m.with_roots(|m, t| {
                    let values: Vec<_> = seen[i..j].iter().map(|(_, v)| *v).collect();
                    let value = list(m, t, &values)?;
                    let value = cell(m, t, value)?;
                    Ok::<_, crate::Error>(RootedAttr {
                        name,
                        pos: SrcPos::NONE,
                        value: r.copy(value),
                    })
                })?);
                i = j;
            }
            set(m, r, attrs)
        }
        "concatMap" => {
            let f = force_function(
                l,
                m,
                r,
                a[0],
                p,
                "while evaluating the first argument passed to builtins.concatMap",
            )?;
            let input = force_list(
                l,
                m,
                r,
                a[1],
                p,
                "while evaluating the second argument passed to builtins.concatMap",
            )?;
            let mut out = Vec::new();
            for index in 0..list_len(&m.heap, input) {
                m.with_roots(|m,t|{
                let elem=list_item(&m.heap,t,input,index);let value=l.call_value_in_frame(m,t,f,&[elem],p)?;
                let arg_pos=determine_pos(l,m,value,determine_pos(l,m,f,p));let rc=cell(m,t,value)?;
                let sub=force_list(l,m,t,rc,arg_pos,"while evaluating the return value of the function passed to builtins.concatMap")?;
                out.extend(elements(&m.heap,r,sub));Ok::<_,crate::Error>(())
            })?;
            }
            list(m, r, &out)
        }
        _ => unreachable!("dispatcher checked builtin name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;

    fn check(expressions: &[&str]) {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let language = Language::new(legacy);
        for stress in [false, true] {
            with_owned_heap(
                brix_gc::Config {
                    workers: 0,
                    chunk_bytes: 256,
                    steady_threshold_bytes: None,
                    trace_collections: false,
                    threshold_bytes: 1024,
                    stress,
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    for source in expressions {
                        machine.with_roots(|machine, roots| {
                            let value = language
                                .eval_str(machine, roots, source.as_bytes(), "/")
                                .unwrap_or_else(|e| panic!("{source}: {}", e.render(true)));
                            assert!(
                                matches!(machine.heap.view(value), ValueView::Bool(true)),
                                "{source}: {:?}",
                                String::from_utf8_lossy(&crate::managed_builtins::print_for_error(
                                    &language, machine, value
                                ))
                            );
                            machine.heap.collect().unwrap();
                            assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                        });
                    }
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn attribute_name_cache_survives_collection_and_hits_do_not_allocate() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        let alpha = language.legacy().intern(b"cacheAlpha");
        let beta = language.legacy().intern(b"cacheBeta");
        with_owned_heap(brix_gc::Config { stress: true, ..Default::default() }, |heap| {
            let mut machine = Machine::new(heap);
            // Each evaluation creates fresh mapAttrs applications. The two
            // immutable name cells must nevertheless be shared by all of them.
            for _ in 0..16 {
                machine.with_roots(|machine, roots| {
                    let value = language.eval_str(machine, roots,
                        b"builtins.mapAttrs (name: value: name) { cacheAlpha = 0; cacheBeta = 1; }", "/").unwrap();
                    let ValueView::Attrs(attrs) = machine.heap.view(value) else { panic!("attrs") };
                    let cells = [attrs.get(alpha).unwrap().value.root(roots), attrs.get(beta).unwrap().value.root(roots)];
                    for (name, cell) in [alpha, beta].into_iter().zip(cells) {
                        let value = language.force(machine, roots, cell, SrcPos::NONE).unwrap();
                        let ValueView::String(value) = machine.heap.view(value) else { panic!("string") };
                        assert_eq!(value.bytes(), language.legacy().symbol_name(name));
                    }
                });
                // No scoped roots from evaluation remain: cache leases alone
                // keep these name cells alive and update them during relocation.
                machine.heap.collect().unwrap();
                assert_eq!(machine.symbol_values.iter().flatten().count(), 2);
            }
            machine.with_roots(|machine, roots| {
                let expected = roots.copy(machine.symbol_values[alpha.0 as usize].as_ref().unwrap().root());
                let used = machine.heap.used_bytes();
                let collections = machine.heap.stats().collections;
                for _ in 0..128 {
                    machine.with_roots(|machine, temporary| {
                        let actual = symbol_cell(&language, machine, temporary, alpha).unwrap();
                        assert!(machine.heap.same_cell(actual, expected));
                    });
                }
                assert_eq!(machine.heap.used_bytes(), used);
                assert_eq!(machine.heap.stats().collections, collections);
            });
        }).unwrap();
    }

    #[test]
    fn managed_math_and_attribute_builtins_collect_without_changing_laziness() {
        check(&[
            "builtins.add 20 22 == 42",
            "builtins.sub 50 8 == 42",
            "builtins.mul 6 7 == 42",
            "builtins.div 85 2 == 42",
            "builtins.div 85 2.0 == 42.5",
            "builtins.bitAnd 7 3 == 3 && builtins.bitOr 4 2 == 6 && builtins.bitXor 7 3 == 4",
            "builtins.ceil 1.1 == 2 && builtins.floor (-1.1) == (-2)",
            "builtins.lessThan [ 1 2 ] [ 1 3 ]",
            "builtins.attrNames { z = throw \"lazy\"; a = 1; } == [ \"a\" \"z\" ]",
            "builtins.attrValues { z = 2; a = 1; } == [ 1 2 ]",
            "builtins.getAttr \"a\" { a = 42; } == 42",
            "builtins.hasAttr \"a\" { a = throw \"lazy\"; }",
            "builtins.isAttrs {} && !(builtins.isAttrs [])",
            "builtins.removeAttrs { a = 1; b = 2; } [ \"a\" \"absent\" ] == { b = 2; }",
            "builtins.listToAttrs [ { name = \"x\"; value = 1; } { name = \"x\"; } ] == { x = 1; }",
            "builtins.intersectAttrs { a = throw \"lazy\"; b = 1; } { a = 42; c = 0; } == { a = 42; }",
            "builtins.catAttrs \"x\" [ { x = 1; } {} { x = 2; } ] == [ 1 2 ]",
            "builtins.mapAttrs false {} == {}",
            "builtins.attrNames (builtins.mapAttrs false { a = throw \"lazy\"; }) == [ \"a\" ]",
            "builtins.unsafeGetAttrPos \"a\" (builtins.mapAttrs (n: v: v) { a = 1; }) == null",
            "builtins.mapAttrs (n: v: n + builtins.toString v) { a = 1; b = 2; } == { a = \"a1\"; b = \"b2\"; }",
            "builtins.attrNames (builtins.mapAttrs (throw \"lazy\") { a = 1; }) == [ \"a\" ]",
            "builtins.filterAttrs (n: v: n == \"b\") { a = throw \"lazy\"; b = 2; } == { b = 2; }",
            "builtins.filterAttrs (throw \"lazy\") {} == {}",
            "builtins.zipAttrsWith (n: vs: vs) [ { x = 1; } { x = 2; y = 3; } ] == { x = [ 1 2 ]; y = [ 3 ]; }",
        ]);
    }

    #[test]
    fn managed_list_builtins_collect_through_callbacks_and_lazy_results() {
        check(&[
            "builtins.isList [] && !(builtins.isList {})",
            "builtins.elemAt [ (throw \"lazy\") 42 ] 1 == 42",
            "builtins.head [ 42 (throw \"lazy\") ] == 42",
            "builtins.tail [ (throw \"lazy\") 42 ] == [ 42 ]",
            "builtins.map (x: x + 1) [ 1 2 3 ] == [ 2 3 4 ]",
            "builtins.map (throw \"lazy\") [] == []",
            "builtins.filter (x: x > 1) [ 1 2 3 ] == [ 2 3 ]",
            "builtins.filter (throw \"lazy\") [] == []",
            "builtins.elem 2 [ 1 2 (throw \"lazy\") ]",
            "builtins.concatLists [ [] [ 1 ] [] [ 2 3 ] ] == [ 1 2 3 ]",
            "builtins.length [ (throw \"lazy\") 2 3 ] == 3",
            "builtins.foldl' (a: x: a + x) 0 [ 1 2 3 ] == 6",
            "builtins.any (x: x) [ false true (throw \"lazy\") ]",
            "builtins.all (x: x) [ true false (throw \"lazy\") ] == false",
            "builtins.genList (x: x * 2) 4 == [ 0 2 4 6 ]",
            "builtins.sort builtins.lessThan [ 3 1 2 1 ] == [ 1 1 2 3 ]",
            "builtins.sort (a: b: a > b) [ 3 1 2 ] == [ 3 2 1 ]",
            "builtins.partition (x: x > 1) [ 1 2 3 ] == { right = [ 2 3 ]; wrong = [ 1 ]; }",
            "builtins.groupBy (x: if x > 1 then \"big\" else \"small\") [ 1 2 3 ] == { big = [ 2 3 ]; small = [ 1 ]; }",
            "builtins.concatMap (x: [ x (x + 1) ]) [ 1 3 ] == [ 1 2 3 4 ]",
        ]);
    }

    #[test]
    fn managed_attribute_equality_checks_names_and_values_in_order() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        let source = r#"{ a = throw "first value"; b = 1; } == { a = 0; c = 1; }"#;
        let expected = language
            .legacy()
            .eval_str(source.as_bytes(), "/")
            .unwrap_err()
            .render(true);
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
                        .unwrap_err();
                    assert_eq!(actual.render(true), expected);
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn managed_data_errors_preserve_numeric_and_ordering_diagnostics() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let legacy = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let language = Language::new(legacy);
        with_owned_heap(brix_gc::Config { workers: 0, chunk_bytes: 256, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: 1024, stress: true }, |heap| {
            let mut machine = Machine::new(heap);
            for (source, expected) in [
                ("builtins.add 9223372036854775807 1", "integer overflow in adding 9223372036854775807 + 1"),
                ("builtins.div \"bad\" 0", "division by zero"),
                ("builtins.elemAt [] (-1)", "'builtins.elemAt' called with index -1 on a list of size 0"),
                ("builtins.head []", "'builtins.head' called on an empty list"),
                ("builtins.listToAttrs [ { name = \"x\"; } { name = throw \"later-name\"; value = 1; } ]", "later-name"),
            ] {
                machine.with_roots(|machine, roots| {
                    let error = language.eval_str(machine, roots, source.as_bytes(), "/").unwrap_err();
                    assert!(error.render(true).windows(expected.len()).any(|part| part == expected.as_bytes()), "{source}: {}", error.render(true));
                });
            }
        }).unwrap();
    }
}
