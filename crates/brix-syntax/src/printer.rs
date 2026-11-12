//! Printing an AST the way `nix-instantiate --parse` does (`Expr::show`
//! in `nixexpr.cc`).
//!
//! The format is fully parenthesised and canonical: attribute bindings are
//! sorted by name, formals are sorted by name, and strings are re-escaped.
//! It is the oracle the parser tests compare against.
//!
//! The helpers for strings and names (`printLiteralString`,
//! `printIdentifier`, `printAttributeName` from `print.cc`) are also the
//! evaluator's: values and error messages print them the same way.

use crate::ast::{AttrDef, AttrName, AttrSet, AttrValue, BinOp, Expr, Formals};
use crate::float::format_g;
use crate::{STACK_RED_ZONE, STACK_SEGMENT};

/// The output buffer: raw bytes, since Nix strings need not be UTF-8.
type Out = Vec<u8>;

/// Renders `expr` as `nix-instantiate --parse` prints it (without the
/// trailing newline). The output is bytes because string literals are;
/// [`show`] is the same text with invalid UTF-8 replaced.
pub fn show_bytes(expr: &Expr) -> Vec<u8> {
    let mut out = Vec::new();
    show_expr(expr, &mut out);
    out
}

/// [`show_bytes`] as a `String`, lossily.
pub fn show(expr: &Expr) -> String {
    String::from_utf8_lossy(&show_bytes(expr)).into_owned()
}

/// `showAttrSelectionPath`: `a.b."c d"."${e}"`, bytes like the names.
pub fn show_attr_path(path: &[AttrName]) -> Vec<u8> {
    let mut out = Vec::new();
    show_attr_path_into(path, &mut out);
    out
}

fn show_attr_path_into(path: &[AttrName], out: &mut Out) {
    for (i, name) in path.iter().enumerate() {
        if i > 0 {
            out.push(b'.');
        }
        match name {
            AttrName::Static(name) => print_identifier(out, name.as_bytes()),
            AttrName::Dynamic(e) => {
                out.extend_from_slice(b"\"${");
                show_expr(e, out);
                out.extend_from_slice(b"}\"");
            }
        }
    }
}

fn show_expr(expr: &Expr, out: &mut Out) {
    // Grows the stack on demand: the printer recurses as deep as the parser.
    stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, || show_expr_inner(expr, out))
}

fn show_expr_inner(expr: &Expr, out: &mut Out) {
    match expr {
        Expr::Int { value, .. } => out.extend_from_slice(value.to_string().as_bytes()),
        Expr::Float { value, .. } => out.extend_from_slice(format_g(*value).as_bytes()),
        Expr::String { value, .. } => print_literal_string(out, value, usize::MAX),
        Expr::Path { path, .. } => out.extend_from_slice(path),
        Expr::Var { name, .. } => print_identifier(out, name.as_bytes()),
        Expr::Select {
            subject,
            path,
            default,
            ..
        } => {
            out.push(b'(');
            show_expr(subject, out);
            out.extend_from_slice(b").");
            show_attr_path_into(path, out);
            if let Some(default) = default {
                out.extend_from_slice(b" or (");
                show_expr(default, out);
                out.push(b')');
            }
        }
        Expr::HasAttr { subject, path } => {
            out.extend_from_slice(b"((");
            show_expr(subject, out);
            out.extend_from_slice(b") ? ");
            show_attr_path_into(path, out);
            out.push(b')');
        }
        Expr::Attrs(set) => {
            if set.recursive {
                out.extend_from_slice(b"rec ");
            }
            out.extend_from_slice(b"{ ");
            show_bindings(set, out);
            out.push(b'}');
        }
        Expr::List { elems, .. } => {
            out.extend_from_slice(b"[ ");
            for e in elems {
                out.push(b'(');
                show_expr(e, out);
                out.extend_from_slice(b") ");
            }
            out.push(b']');
        }
        Expr::Lambda {
            arg, formals, body, ..
        } => {
            out.push(b'(');
            if let Some(formals) = formals {
                show_formals(formals, out);
                if arg.is_some() {
                    out.extend_from_slice(b" @ ");
                }
            }
            if let Some(arg) = arg {
                print_identifier(out, arg.as_bytes());
            }
            out.extend_from_slice(b": ");
            show_expr(body, out);
            out.push(b')');
        }
        Expr::Call { fun, args, .. } => {
            out.push(b'(');
            show_expr(fun, out);
            for arg in args {
                out.push(b' ');
                show_expr(arg, out);
            }
            out.push(b')');
        }
        Expr::Let { attrs, body } => {
            out.extend_from_slice(b"(let ");
            show_bindings(attrs, out);
            out.extend_from_slice(b"in ");
            show_expr(body, out);
            out.push(b')');
        }
        Expr::With { attrs, body, .. } => {
            out.extend_from_slice(b"(with ");
            show_expr(attrs, out);
            out.extend_from_slice(b"; ");
            show_expr(body, out);
            out.push(b')');
        }
        Expr::If {
            cond, then_, else_, ..
        } => {
            out.extend_from_slice(b"(if ");
            show_expr(cond, out);
            out.extend_from_slice(b" then ");
            show_expr(then_, out);
            out.extend_from_slice(b" else ");
            show_expr(else_, out);
            out.push(b')');
        }
        Expr::Assert { cond, body, .. } => {
            out.extend_from_slice(b"assert ");
            show_expr(cond, out);
            out.extend_from_slice(b"; ");
            show_expr(body, out);
        }
        Expr::Not(e) => {
            out.extend_from_slice(b"(! ");
            show_expr(e, out);
            out.push(b')');
        }
        Expr::BinOp { op, lhs, rhs, .. } => {
            out.push(b'(');
            show_expr(lhs, out);
            out.push(b' ');
            out.extend_from_slice(match op {
                BinOp::Eq => b"==",
                BinOp::NEq => b"!=",
                BinOp::And => b"&&",
                BinOp::Or => b"||",
                BinOp::Impl => b"->",
                BinOp::Update => b"//",
                BinOp::ConcatLists => b"++",
            });
            out.push(b' ');
            show_expr(rhs, out);
            out.push(b')');
        }
        Expr::ConcatStrings { parts, .. } => {
            out.push(b'(');
            for (i, (_, e)) in parts.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b" + ");
                }
                show_expr(e, out);
            }
            out.push(b')');
        }
        Expr::CurPos { .. } => out.extend_from_slice(b"__curPos"),
    }
}

/// `ExprAttrs::showBindings`: `inherit a b; inherit (e) c; x = 1; "${d}" = 2; `
/// with every group sorted by name and a trailing space.
fn show_bindings(set: &AttrSet, out: &mut Out) {
    let mut sorted: Vec<&AttrDef> = set.attrs.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    let inherits: Vec<&&AttrDef> = sorted
        .iter()
        .filter(|d| d.value == AttrValue::Inherited)
        .collect();
    if !inherits.is_empty() {
        out.extend_from_slice(b"inherit");
        for d in inherits {
            out.push(b' ');
            print_identifier(out, d.name.as_bytes());
        }
        out.extend_from_slice(b"; ");
    }

    for (index, from) in set.inherit_from.iter().enumerate() {
        let names: Vec<&&AttrDef> = sorted
            .iter()
            .filter(|d| {
                d.value
                    == AttrValue::InheritedFrom {
                        index: index as u32,
                    }
            })
            .collect();
        if names.is_empty() {
            continue;
        }
        out.extend_from_slice(b"inherit (");
        show_expr(from, out);
        out.push(b')');
        for d in names {
            out.push(b' ');
            print_identifier(out, d.name.as_bytes());
        }
        out.extend_from_slice(b"; ");
    }

    for d in &sorted {
        if let AttrValue::Plain(e) = &d.value {
            print_identifier(out, d.name.as_bytes());
            out.extend_from_slice(b" = ");
            show_expr(e, out);
            out.extend_from_slice(b"; ");
        }
    }

    for d in &set.dynamic_attrs {
        out.extend_from_slice(b"\"${");
        show_expr(&d.name, out);
        out.extend_from_slice(b"}\" = ");
        show_expr(&d.value, out);
        out.extend_from_slice(b"; ");
    }
}

fn show_formals(formals: &Formals, out: &mut Out) {
    out.extend_from_slice(b"{ ");
    let mut sorted: Vec<_> = formals.formals.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut first = true;
    for f in sorted {
        if !first {
            out.extend_from_slice(b", ");
        }
        first = false;
        print_identifier(out, f.name.as_bytes());
        if let Some(def) = &f.default {
            out.extend_from_slice(b" ? ");
            show_expr(def, out);
        }
    }
    if formals.ellipsis {
        if !first {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(b"...");
    }
    out.extend_from_slice(b" }");
}

/// `printLiteralString` (`print.cc`): `s` quoted and escaped so that it
/// parses back to the same string, cut after `max_length` bytes with
/// ` «n bytes elided»` (possibly inside a multi-byte character: Nix counts
/// and cuts bytes). Only ASCII is escaped; every other byte is written as
/// it is, so the output is UTF-8 exactly when `s` is (and the cut is not
/// inside a character).
pub fn print_literal_string(out: &mut Vec<u8>, s: &[u8], max_length: usize) {
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        if i >= max_length {
            out.extend_from_slice(b"\" ");
            let rest = s.len() - i;
            let unit = if rest == 1 { "byte" } else { "bytes" };
            out.extend_from_slice(format!("«{rest} {unit} elided»").as_bytes());
            return;
        }
        match c {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', c]),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'$' if s.get(i + 1) == Some(&b'{') => out.extend_from_slice(b"\\$"),
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// `printIdentifier`: plain when it lexes as an identifier and is not a
/// keyword, otherwise quoted.
pub fn print_identifier(out: &mut Vec<u8>, s: &[u8]) {
    if s.is_empty() {
        out.extend_from_slice(b"\"\"");
    } else if is_reserved_keyword(s) {
        out.push(b'"');
        out.extend_from_slice(s);
        out.push(b'"');
    } else if is_identifier(s) {
        out.extend_from_slice(s);
    } else {
        print_literal_string(out, s, usize::MAX);
    }
}

/// `printAttributeName`: plain when `isVarName` (an identifier, keyword
/// or not, that does not start with a digit, `-` or `'`), otherwise
/// quoted.
pub fn print_attribute_name(out: &mut Vec<u8>, s: &[u8]) {
    if is_var_name(s) {
        out.extend_from_slice(s);
    } else {
        print_literal_string(out, s, usize::MAX);
    }
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'\'' || c == b'-'
}

/// What `printIdentifier` prints bare (`s` is not empty).
fn is_identifier(s: &[u8]) -> bool {
    (s[0].is_ascii_alphabetic() || s[0] == b'_') && s.iter().all(|&c| is_ident_char(c))
}

/// `isVarName`.
fn is_var_name(s: &[u8]) -> bool {
    match s.first() {
        None => false,
        Some(c) if c.is_ascii_digit() || *c == b'-' || *c == b'\'' => false,
        Some(_) => !is_reserved_keyword(s) && s.iter().all(|&c| is_ident_char(c)),
    }
}

/// `isReservedKeyword`.
fn is_reserved_keyword(s: &[u8]) -> bool {
    matches!(
        s,
        b"if" | b"then" | b"else" | b"assert" | b"with" | b"let" | b"in" | b"rec" | b"inherit"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Pos;

    fn s(text: &[u8], max_length: usize) -> Vec<u8> {
        let mut out = Vec::new();
        print_literal_string(&mut out, text, max_length);
        out
    }

    #[test]
    fn literal_strings_are_escaped() {
        assert_eq!(s(b"a\"b\\c\nd\re\tf", usize::MAX), br#""a\"b\\c\nd\re\tf""#);
        assert_eq!(s(b"a${b}$c$", usize::MAX), br#""a\${b}$c$""#);
        assert_eq!(s(b"", usize::MAX), b"\"\"");
    }

    #[test]
    fn literal_strings_are_cut_after_max_length_bytes() {
        assert_eq!(s(b"abcdef", 3), "\"abc\" «3 bytes elided»".as_bytes());
        assert_eq!(s(b"abcd", 3), "\"abc\" «1 byte elided»".as_bytes());
        assert_eq!(s(b"abc", 3), b"\"abc\"");
        // Bytes, not characters, are counted and cut.
        assert_eq!(
            s("aéé".as_bytes(), 2),
            b"\"a\xC3\" \xC2\xAB3 bytes elided\xC2\xBB"
        );
        // Bytes that are not UTF-8 are kept as they are.
        assert_eq!(
            s(b"\xC3\"\xE2\x82${", usize::MAX),
            b"\"\xC3\\\"\xE2\x82\\${\""
        );
    }

    #[test]
    fn invalid_utf8_passes_through() {
        let e = Expr::String {
            pos: Pos::NONE,
            value: b"\xff".to_vec(),
        };
        assert_eq!(show_bytes(&e), b"\"\xff\"");
        assert_eq!(show(&e), "\"\u{fffd}\"");
    }

    #[test]
    fn identifiers() {
        let p = |s: &[u8]| {
            let mut out = Vec::new();
            print_identifier(&mut out, s);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(p(b"a-b'"), "a-b'");
        assert_eq!(p(b"or"), "or");
        assert_eq!(p(b"_x"), "_x");
        assert_eq!(p(b"if"), "\"if\"");
        assert_eq!(p(b"in"), "\"in\"");
        assert_eq!(p(b"1a"), "\"1a\"");
        assert_eq!(p(b"a b"), "\"a b\"");
        assert_eq!(p(b""), "\"\"");
    }

    #[test]
    fn attribute_names() {
        let p = |s: &[u8]| {
            let mut out = Vec::new();
            print_attribute_name(&mut out, s);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(p(b"a'"), "a'");
        assert_eq!(p(b"or"), "or");
        assert_eq!(p(b"'a"), "\"'a\"");
        assert_eq!(p(b"-a"), "\"-a\"");
        assert_eq!(p(b"1a"), "\"1a\"");
        assert_eq!(p(b"rec"), "\"rec\"");
        assert_eq!(p(b""), "\"\"");
    }

    #[test]
    fn assert_has_no_parentheses() {
        let e = Expr::Assert {
            pos: Pos(0),
            cond: Box::new(Expr::Var {
                pos: Pos(7),
                name: "x".into(),
            }),
            body: Box::new(Expr::Int {
                pos: Pos(10),
                value: 1,
            }),
        };
        assert_eq!(show(&e), "assert x; 1");
    }
}
