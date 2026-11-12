//! Printing values the way Nix does.
//!
//! Two printers exist in Nix: `printAmbiguous` (`nix-instantiate --eval`
//! and `builtins.trace`) and the newer `ValuePrinter` (`nix eval`, error
//! messages). Both are reproduced here, byte for byte: strings are written
//! as their bytes (`printLiteralString`, which escapes only ASCII), so the
//! output is UTF-8 exactly when the strings in the value are. Floats are
//! C's `%g` ([`brix_syntax::float::format_g`]); `toString` uses `%f`
//! ([`format_float_f`]).
//!
//! Both recurse as deep as the value nests, up to `max-call-depth`, with
//! the stack check that forcing and calling make ([`Evaluator::with_stack`])
//! on every level.

use std::collections::HashSet;
use std::io::Write as _;

use brix_syntax::float::format_g;
use brix_syntax::printer::{print_attribute_name, print_identifier, print_literal_string};
use bstr::BString;

use crate::error::ErrorKind;
use crate::evaluator::Evaluator;
use crate::symbol::sym;
use crate::value::{primop_app_primop, Attr, SrcPos, Value, ValueRef};
use crate::Result;

/// `PrintOptions` in `print-options.hh`.
#[derive(Clone, Debug)]
pub struct PrintOptions {
    pub force: bool,
    pub derivation_paths: bool,
    pub track_repeated: bool,
    pub max_depth: usize,
    pub max_attrs: usize,
    pub max_list_items: usize,
    pub max_string_length: usize,
    pub pretty_indent: usize,
    /// Print errors inline as `«error: …»` instead of propagating them.
    pub print_errors: bool,
}

impl Default for PrintOptions {
    fn default() -> PrintOptions {
        PrintOptions {
            force: false,
            derivation_paths: false,
            track_repeated: true,
            max_depth: usize::MAX,
            max_attrs: usize::MAX,
            max_list_items: usize::MAX,
            max_string_length: usize::MAX,
            pretty_indent: 0,
            print_errors: true,
        }
    }
}

impl PrintOptions {
    /// `errorPrintOptions`: how values appear inside error messages.
    pub fn for_errors() -> PrintOptions {
        PrintOptions {
            max_depth: 10,
            max_attrs: 10,
            max_list_items: 10,
            max_string_length: 1024,
            ..PrintOptions::default()
        }
    }

    /// What `nix eval` uses.
    pub fn for_eval() -> PrintOptions {
        PrintOptions {
            force: true,
            derivation_paths: true,
            ..PrintOptions::default()
        }
    }
}

/// C's `%f`: six decimals.
pub fn format_float_f(f: f64) -> String {
    if f.is_nan() {
        return if f.is_sign_negative() {
            "-nan".into()
        } else {
            "nan".into()
        };
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".into() } else { "inf".into() };
    }
    format!("{f:.6}")
}

/// Appends `text` (ASCII or UTF-8 text such as a number or a position).
fn push(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

/// `nix-instantiate --eval` shows an integer as `std::ostream` does.
fn push_int(out: &mut Vec<u8>, i: i64) {
    write!(out, "{i}").expect("writing to a Vec cannot fail");
}

impl<'a> Evaluator<'a> {
    /// A value as it appears inside an error message (`ValuePrinter` with
    /// `errorPrintOptions`, errors printed inline, nothing forced).
    pub fn print_for_error(&self, v: Value<'a>) -> BString {
        let cell = self.alloc_cell(v);
        let mut out = Vec::new();
        let mut p = Printer::new(self, PrintOptions::for_errors());
        // Errors print inline, and the only one that propagates is a
        // stack overflow, after which the text so far is the message's.
        if let Err(e) = p.print(&mut out, cell, 0) {
            debug_assert_eq!(e.kind, ErrorKind::StackOverflow);
        }
        out.into()
    }

    /// `printValue` with the given options.
    pub fn print_value(&self, cell: ValueRef<'a>, options: &PrintOptions) -> Result<BString> {
        let mut out = Vec::new();
        let mut p = Printer::new(self, options.clone());
        p.print(&mut out, cell, 0)?;
        Ok(out.into())
    }

    /// `printAmbiguous`: the `nix-instantiate --eval` format.
    pub fn print_ambiguous(&self, cell: ValueRef<'a>) -> Result<BString> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.print_ambiguous_inner(&mut out, cell, &mut seen, 0)?;
        Ok(out.into())
    }

    fn print_ambiguous_inner(
        &self,
        out: &mut Vec<u8>,
        cell: ValueRef<'a>,
        seen: &mut HashSet<usize>,
        depth: usize,
    ) -> Result<()> {
        self.with_stack(|| self.print_ambiguous_level(out, cell, seen, depth))
    }

    fn print_ambiguous_level(
        &self,
        out: &mut Vec<u8>,
        cell: ValueRef<'a>,
        seen: &mut HashSet<usize>,
        depth: usize,
    ) -> Result<()> {
        let v = cell.get();
        if depth > self.config.max_call_depth as usize {
            return Err(self.error_at(
                ErrorKind::StackOverflow,
                "stack overflow; max-call-depth exceeded",
                self.determine_pos(v, SrcPos::NONE),
            ));
        }
        match v {
            Value::Int(i) => push_int(out, i),
            Value::Bool(b) => push(out, if b { "true" } else { "false" }),
            Value::String(s) => print_literal_string(out, self.str_bytes(s)?, usize::MAX),
            Value::Path(p) => out.extend_from_slice(&self.show_path(p)),
            Value::Null => push(out, "null"),
            Value::Attrs(b) => {
                if !b.is_empty() && !seen.insert(b.addr()) {
                    push(out, "«repeated»");
                } else {
                    push(out, "{ ");
                    for attr in self.lexicographic_attrs(b) {
                        print_identifier(out, self.symbol_name(attr.name));
                        push(out, " = ");
                        self.print_ambiguous_inner(out, attr.value, seen, depth + 1)?;
                        push(out, "; ");
                    }
                    out.push(b'}');
                }
            }
            Value::List(l) => {
                if !l.elems.is_empty() && !seen.insert(cell as *const _ as usize) {
                    push(out, "«repeated»");
                } else {
                    push(out, "[ ");
                    for &e in l.elems {
                        self.print_ambiguous_inner(out, e, seen, depth + 1)?;
                        out.push(b' ');
                    }
                    out.push(b']');
                }
            }
            Value::Thunk { .. } | Value::App(_) => push(out, "<CODE>"),
            Value::Blackhole => push(out, "«potential infinite recursion»"),
            Value::Failed(_) => push(out, "«failed»"),
            Value::Lambda { .. } => push(out, "<LAMBDA>"),
            Value::PrimOp(_) => push(out, "<PRIMOP>"),
            Value::PrimOpApp(_) => push(out, "<PRIMOP-APP>"),
            Value::Float(f) => push(out, &format_g(f)),
        }
        Ok(())
    }

    /// Attributes in name order (`Bindings::lexicographicOrder`).
    pub fn lexicographic_attrs(&self, b: crate::value::Bindings<'a>) -> Vec<&'a Attr<'a>> {
        b.lexicographic(self.keys)
    }
}

/// `Printer` in `print.cc`.
struct Printer<'e, 'a> {
    ev: &'e Evaluator<'a>,
    options: PrintOptions,
    seen: Option<HashSet<usize>>,
    total_attrs: usize,
    total_list_items: usize,
    indent: Vec<u8>,
}

impl<'e, 'a> Printer<'e, 'a> {
    fn new(ev: &'e Evaluator<'a>, options: PrintOptions) -> Printer<'e, 'a> {
        let seen = if options.track_repeated {
            Some(HashSet::new())
        } else {
            None
        };
        Printer {
            ev,
            options,
            seen,
            total_attrs: 0,
            total_list_items: 0,
            indent: Vec::new(),
        }
    }

    fn pretty(&self) -> bool {
        self.options.pretty_indent > 0
    }

    fn space(&self, out: &mut Vec<u8>, pretty: bool) {
        if pretty {
            out.push(b'\n');
            out.extend_from_slice(&self.indent);
        } else {
            out.push(b' ');
        }
    }

    fn increase_indent(&mut self) {
        let n = self.indent.len() + self.options.pretty_indent;
        self.indent.resize(n, b' ');
    }

    fn decrease_indent(&mut self) {
        let n = self.indent.len() - self.options.pretty_indent;
        self.indent.truncate(n);
    }

    fn print(&mut self, out: &mut Vec<u8>, cell: ValueRef<'a>, depth: usize) -> Result<()> {
        let ev = self.ev;
        ev.with_stack(|| self.print_level(out, cell, depth))
    }

    fn print_level(&mut self, out: &mut Vec<u8>, cell: ValueRef<'a>, depth: usize) -> Result<()> {
        let ev = self.ev;
        if depth > ev.config.max_call_depth as usize {
            return Err(ev.error_at(
                ErrorKind::StackOverflow,
                "stack overflow; max-call-depth exceeded",
                ev.determine_pos(cell.get(), SrcPos::NONE),
            ));
        }
        let r: Result<()> = (|| {
            let v = if self.options.force {
                ev.force(cell, ev.determine_pos(cell.get(), SrcPos::NONE))?
            } else {
                cell.get()
            };
            match v {
                Value::Int(i) => push_int(out, i),
                Value::Float(f) => push(out, &format_g(f)),
                Value::Bool(b) => push(out, if b { "true" } else { "false" }),
                Value::String(s) => {
                    print_literal_string(out, ev.str_bytes(s)?, self.options.max_string_length)
                }
                Value::Path(p) => out.extend_from_slice(&ev.show_path(p)),
                Value::Null => push(out, "null"),
                Value::Attrs(b) => self.print_attrs(out, b, depth)?,
                Value::List(_) => self.print_list(out, cell, depth)?,
                Value::Lambda { node, .. } => {
                    push(out, "«lambda");
                    let (code, lam) = ev.lambda_def(node);
                    if let Some(n) = lam.name {
                        out.push(b' ');
                        out.extend_from_slice(ev.symbol_name(n));
                    }
                    push(out, " @ ");
                    match ev.position(ev.src_pos(code, lam.pos)) {
                        Some(p) => out.extend_from_slice(&p.display()),
                        None => push(out, "«none»:0"),
                    }
                    push(out, "»");
                }
                Value::PrimOp(p) => push(out, &format!("«primop {}»", p.name)),
                Value::PrimOpApp(app) => match primop_app_primop(app) {
                    Some(p) => push(out, &format!("«partially applied primop {}»", p.name)),
                    None => push(out, "«partially applied primop»"),
                },
                Value::Blackhole => push(out, "«potential infinite recursion»"),
                Value::Thunk { .. } | Value::App(_) => push(out, "«thunk»"),
                Value::Failed(_) => push(out, "«failed»"),
            }
            Ok(())
        })();
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.kind == ErrorKind::StackOverflow => Err(e),
            Err(e) => {
                if !self.options.print_errors {
                    return Err(e);
                }
                push(out, "«error: ");
                out.extend_from_slice(&e.msg);
                push(out, "»");
                Ok(())
            }
        }
    }

    fn should_pretty_print(&self, items: &[ValueRef<'a>]) -> Result<bool> {
        if !self.pretty() || items.is_empty() {
            return Ok(false);
        }
        if items.len() > 1 {
            return Ok(true);
        }
        let item = items[0];
        let v = self
            .ev
            .force(item, self.ev.determine_pos(item.get(), SrcPos::NONE))?;
        Ok(matches!(v, Value::List(_) | Value::Attrs(_)))
    }

    fn print_attrs(
        &mut self,
        out: &mut Vec<u8>,
        b: crate::value::Bindings<'a>,
        depth: usize,
    ) -> Result<()> {
        let ev = self.ev;
        if let Some(seen) = &mut self.seen {
            if !seen.insert(b.addr()) {
                push(out, "«repeated»");
                return Ok(());
            }
        }
        if self.options.force
            && self.options.derivation_paths
            && ev.is_derivation(Value::Attrs(b))?
        {
            let mut store_path = None;
            if let Some(a) = b.get(sym::DRV_PATH) {
                let mut ctx = crate::string_context::StringContext::new();
                store_path = Some(ev.coerce_to_store_path(
                    a.pos,
                    a.value,
                    &mut ctx,
                    "while evaluating the drvPath of a derivation",
                )?);
            }
            push(out, "«derivation");
            if let Some(sp) = store_path {
                out.push(b' ');
                push(out, &ev.config.store_dir.display(&sp));
            }
            push(out, "»");
            return Ok(());
        }
        if depth >= self.options.max_depth {
            push(out, "{ ... }");
            return Ok(());
        }
        self.increase_indent();
        out.push(b'{');
        // In name order, as Nix sorts them; when truncating, the important
        // names (`type`, `_type`) move to the front (a stable partition
        // keeps the rest in order, as `ImportantFirstAttrNameCmp` does).
        let mut sorted: Vec<(&'a [u8], &'a Attr<'a>)> = b
            .lexicographic(ev.keys)
            .into_iter()
            .map(|a| (ev.symbol_name(a.name), a))
            .collect();
        if self.options.max_attrs != usize::MAX {
            let imp = |n: &[u8]| n == b"type" || n == b"_type";
            sorted.sort_by_key(|(n, _)| !imp(n));
        }
        let items: Vec<ValueRef<'a>> = sorted.iter().map(|(_, a)| a.value).collect();
        let pretty = self.should_pretty_print(&items)?;
        for (current, (name, attr)) in sorted.iter().enumerate() {
            self.space(out, pretty);
            if self.total_attrs >= self.options.max_attrs {
                let rest = sorted.len() - current;
                let unit = if rest == 1 { "attribute" } else { "attributes" };
                push(out, &format!("«{rest} {unit} elided»"));
                break;
            }
            print_attribute_name(out, name);
            push(out, " = ");
            self.print(out, attr.value, depth + 1)?;
            out.push(b';');
            self.total_attrs += 1;
        }
        self.decrease_indent();
        self.space(out, pretty);
        out.push(b'}');
        Ok(())
    }

    fn print_list(&mut self, out: &mut Vec<u8>, cell: ValueRef<'a>, depth: usize) -> Result<()> {
        let Value::List(l) = cell.get() else {
            unreachable!("print_list is called on lists")
        };
        if let Some(seen) = &mut self.seen {
            if !l.elems.is_empty() && !seen.insert(cell as *const _ as usize) {
                push(out, "«repeated»");
                return Ok(());
            }
        }
        if depth >= self.options.max_depth {
            push(out, "[ ... ]");
            return Ok(());
        }
        self.increase_indent();
        out.push(b'[');
        let pretty = self.should_pretty_print(l.elems)?;
        for (current, &elem) in l.elems.iter().enumerate() {
            self.space(out, pretty);
            if self.total_list_items >= self.options.max_list_items {
                let rest = l.elems.len() - current;
                let unit = if rest == 1 { "item" } else { "items" };
                push(out, &format!("«{rest} {unit} elided»"));
                break;
            }
            self.print(out, elem, depth + 1)?;
            self.total_list_items += 1;
        }
        self.decrease_indent();
        self.space(out, pretty);
        out.push(b']');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_f() {
        assert_eq!(format_float_f(1.5), "1.500000");
        assert_eq!(format_float_f(2.0), "2.000000");
        assert_eq!(format_float_f(1e-7), "0.000000");
        assert_eq!(format_float_f(-f64::NAN), "-nan");
    }
}
