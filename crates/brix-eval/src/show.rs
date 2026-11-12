//! Printing compiled expressions back as source (`Expr::show`).
//!
//! Used for `assert` failure messages ("assertion 'x == 1' failed") and
//! for attribute paths in traces. Output matches `nixexpr.cc` for the
//! desugared tree, which is what Nix prints too (it shows `(__sub a b)`
//! for `a - b`).

use brix_syntax::float::format_g;
use brix_syntax::printer::{print_identifier, print_literal_string};

use crate::code::LinkedCode;
use crate::evaluator::Evaluator;
use crate::ir::{AttrKind, AttrName, BinOp, Node, NodeId};

pub fn show_node(ev: &Evaluator<'_>, code: &LinkedCode<'_>, node: NodeId) -> Vec<u8> {
    let mut out = Vec::new();
    show(ev, code, node, &mut out);
    out
}

fn ident(ev: &Evaluator<'_>, out: &mut Vec<u8>, sym: crate::symbol::Symbol) {
    print_identifier(out, ev.symbol_name(sym));
}

fn show_attr_path(
    ev: &Evaluator<'_>,
    code: &LinkedCode<'_>,
    names: &[AttrName],
    out: &mut Vec<u8>,
) {
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(b'.');
        }
        match *name {
            AttrName::Static(s) => ident(ev, out, s),
            AttrName::Dynamic(e) => {
                out.extend_from_slice(b"\"${");
                show(ev, code, e, out);
                out.extend_from_slice(b"}\"");
            }
        }
    }
}

fn show_bindings(
    ev: &Evaluator<'_>,
    code: &LinkedCode<'_>,
    set: &crate::code::AttrSet<'_>,
    out: &mut Vec<u8>,
) {
    let mut sorted: Vec<&crate::ir::AttrDef> = set.attrs.iter().collect();
    sorted.sort_by(|a, b| ev.symbol_name(a.name).cmp(ev.symbol_name(b.name)));
    let inherits: Vec<_> = sorted
        .iter()
        .filter(|d| d.kind == AttrKind::Inherited)
        .map(|d| d.name)
        .collect();
    let mut inherits_from: std::collections::BTreeMap<u32, Vec<crate::symbol::Symbol>> =
        std::collections::BTreeMap::new();
    for d in &sorted {
        if d.kind == AttrKind::InheritedFrom {
            if let Node::Select { subject, .. } = *code.node(d.expr) {
                if let Node::InheritFrom { displ, .. } = *code.node(subject) {
                    inherits_from.entry(displ).or_default().push(d.name);
                }
            }
        }
    }
    if !inherits.is_empty() {
        out.extend_from_slice("inherit".as_bytes());
        for s in inherits {
            out.push(b' ');
            ident(ev, out, s);
        }
        out.extend_from_slice("; ".as_bytes());
    }
    for (from, syms) in inherits_from {
        out.extend_from_slice("inherit (".as_bytes());
        show(ev, code, set.inherit_from[from as usize], out);
        out.push(b')');
        for s in syms {
            out.push(b' ');
            ident(ev, out, s);
        }
        out.extend_from_slice("; ".as_bytes());
    }
    for d in &sorted {
        if d.kind == AttrKind::Plain {
            ident(ev, out, d.name);
            out.extend_from_slice(" = ".as_bytes());
            show(ev, code, d.expr, out);
            out.extend_from_slice("; ".as_bytes());
        }
    }
    for d in set.dynamic_attrs {
        out.extend_from_slice(b"\"${");
        show(ev, code, d.name, out);
        out.extend_from_slice(b"}\" = ");
        show(ev, code, d.value, out);
        out.extend_from_slice("; ".as_bytes());
    }
}

fn show(ev: &Evaluator<'_>, code: &LinkedCode<'_>, node: NodeId, out: &mut Vec<u8>) {
    // The expression nests as deep as the parser let it.
    ev.with_stack(|| show_level(ev, code, node, out))
}

fn show_level(ev: &Evaluator<'_>, code: &LinkedCode<'_>, node: NodeId, out: &mut Vec<u8>) {
    match *code.node(node) {
        Node::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Node::Float(f) => out.extend_from_slice(format_g(f).as_bytes()),
        Node::Str(lit) => print_literal_string(out, code.string(lit), usize::MAX),
        Node::Path { lit, .. } => out.extend_from_slice(code.string(lit)),
        Node::Var { name, .. } => ident(ev, out, name),
        Node::InheritFrom { .. } => out.extend_from_slice("«inherit-from»".as_bytes()),
        Node::Select {
            subject,
            path,
            default,
            ..
        } => {
            out.push(b'(');
            show(ev, code, subject, out);
            out.extend_from_slice(").".as_bytes());
            show_attr_path(ev, code, code.attr_names_in(path), out);
            if let Some(d) = default {
                out.extend_from_slice(" or (".as_bytes());
                show(ev, code, d, out);
                out.push(b')');
            }
        }
        Node::HasAttr { subject, path } => {
            out.extend_from_slice("((".as_bytes());
            show(ev, code, subject, out);
            out.extend_from_slice(") ? ".as_bytes());
            show_attr_path(ev, code, code.attr_names_in(path), out);
            out.push(b')');
        }
        Node::Attrs(id) => {
            let set = code.attr_set(id);
            if set.recursive {
                out.extend_from_slice("rec ".as_bytes());
            }
            out.extend_from_slice("{ ".as_bytes());
            show_bindings(ev, code, set, out);
            out.push(b'}');
        }
        Node::List { elems } => {
            out.extend_from_slice("[ ".as_bytes());
            for &e in code.nodes_in(elems) {
                out.push(b'(');
                show(ev, code, e, out);
                out.extend_from_slice(") ".as_bytes());
            }
            out.push(b']');
        }
        Node::Lambda(id) => {
            let lam = code.lambda(id);
            out.push(b'(');
            if let Some(formals) = &lam.formals {
                out.extend_from_slice("{ ".as_bytes());
                let mut fs: Vec<&crate::ir::Formal> = formals.formals.iter().collect();
                fs.sort_by(|a, b| ev.symbol_name(a.name).cmp(ev.symbol_name(b.name)));
                let mut first = true;
                for f in fs {
                    if !first {
                        out.extend_from_slice(", ".as_bytes());
                    }
                    first = false;
                    ident(ev, out, f.name);
                    if let Some(d) = f.default {
                        out.extend_from_slice(" ? ".as_bytes());
                        show(ev, code, d, out);
                    }
                }
                if formals.ellipsis {
                    if !first {
                        out.extend_from_slice(", ".as_bytes());
                    }
                    out.extend_from_slice("...".as_bytes());
                }
                out.extend_from_slice(" }".as_bytes());
                if lam.arg.is_some() {
                    out.extend_from_slice(" @ ".as_bytes());
                }
            }
            if let Some(a) = lam.arg {
                ident(ev, out, a);
            }
            out.extend_from_slice(": ".as_bytes());
            show(ev, code, lam.body, out);
            out.push(b')');
        }
        Node::Call { fun, args, .. } => {
            out.push(b'(');
            show(ev, code, fun, out);
            for &a in code.nodes_in(args) {
                out.push(b' ');
                show(ev, code, a, out);
            }
            out.push(b')');
        }
        Node::Let { attrs, body } => {
            out.extend_from_slice("(let ".as_bytes());
            show_bindings(ev, code, code.attr_set(attrs), out);
            out.extend_from_slice("in ".as_bytes());
            show(ev, code, body, out);
            out.push(b')');
        }
        Node::With { attrs, body, .. } => {
            out.extend_from_slice("(with ".as_bytes());
            show(ev, code, attrs, out);
            out.extend_from_slice("; ".as_bytes());
            show(ev, code, body, out);
            out.push(b')');
        }
        Node::If {
            cond, then_, else_, ..
        } => {
            out.extend_from_slice("(if ".as_bytes());
            show(ev, code, cond, out);
            out.extend_from_slice(" then ".as_bytes());
            show(ev, code, then_, out);
            out.extend_from_slice(" else ".as_bytes());
            show(ev, code, else_, out);
            out.push(b')');
        }
        Node::Assert { cond, body, .. } => {
            out.extend_from_slice("assert ".as_bytes());
            show(ev, code, cond, out);
            out.extend_from_slice("; ".as_bytes());
            show(ev, code, body, out);
        }
        Node::Not(e) => {
            out.extend_from_slice("(! ".as_bytes());
            show(ev, code, e, out);
            out.push(b')');
        }
        Node::BinOp { op, lhs, rhs, .. } => {
            out.push(b'(');
            show(ev, code, lhs, out);
            out.extend_from_slice(
                match op {
                    BinOp::Eq => " == ",
                    BinOp::NEq => " != ",
                    BinOp::And => " && ",
                    BinOp::Or => " || ",
                    BinOp::Impl => " -> ",
                    BinOp::Update => " // ",
                    BinOp::ConcatLists => " ++ ",
                }
                .as_bytes(),
            );
            show(ev, code, rhs, out);
            out.push(b')');
        }
        Node::ConcatStrings { parts, .. } => {
            out.push(b'(');
            for (i, &(_, e)) in code.parts_in(parts).iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(" + ".as_bytes());
                }
                show(ev, code, e, out);
            }
            out.push(b')');
        }
        Node::CurPos(_) => out.extend_from_slice("__curPos".as_bytes()),
    }
}
