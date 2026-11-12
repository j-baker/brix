//! `builtins.toJSON` / `builtins.fromJSON` (`value-to-json.cc`,
//! `json-to-value.cc`).
//!
//! Output matches nlohmann::json's compact `dump()`: object keys in
//! lexicographic order, control characters escaped as `\uXXXX`, floats
//! in the shortest round-trip form with nlohmann's choice between fixed
//! and exponential notation, and NaN/infinity as `null`.

use std::fmt::Write as _;

use brix_store::json::{Sax, SaxError};

use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::{sym, Symbol};
use crate::value::{Attr, SrcPos, Value, ValueRef};
use crate::Result;

/// nlohmann's `dump_float`: shortest round-trip digits, then fixed
/// notation when the number has at most 15 integer digits and at most 4
/// leading zeros after the point, else exponential with a two-digit
/// exponent.
pub fn format_json_float(f: f64) -> String {
    if !f.is_finite() {
        return "null".to_owned();
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // nlohmann's Grisu2 digits, not Rust's shortest ones: they differ in
    // rare cases (see `grisu.rs`).
    let (raw, decimal_exponent) = crate::grisu::grisu2(f.abs());
    let digits = String::from_utf8(raw).expect("ASCII digits");
    let k = digits.len() as i32;
    let n = k + decimal_exponent; // position of the decimal point relative to digits
    let sign = if f < 0.0 { "-" } else { "" };
    const MIN_EXP: i32 = -4;
    const MAX_EXP: i32 = 15;
    let body = if k <= n && n <= MAX_EXP {
        // digits[000].0
        format!("{}{}.0", digits, "0".repeat((n - k) as usize))
    } else if 0 < n && n <= MAX_EXP {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if MIN_EXP < n && n <= 0 {
        format!("0.{}{}", "0".repeat((-n) as usize), digits)
    } else {
        let e = n - 1;
        let mant = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{mant}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    };
    format!("{sign}{body}")
}

/// nlohmann's compact `dump()` of parsed JSON: keys sorted, no
/// whitespace, strings and floats written as `push_escaped` and
/// [`format_json_float`] write them. Nix stores structured attributes in
/// a derivation's `__json` this way.
pub fn dump(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| !n.is_i64() && !n.is_u64()) {
                format_json_float(f)
            } else {
                n.to_string()
            }
        }
        serde_json::Value::String(s) => {
            let mut out = String::new();
            push_escaped(&mut out, s);
            out
        }
        serde_json::Value::Array(items) => {
            format!("[{}]", items.iter().map(dump).collect::<Vec<_>>().join(","))
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|k| {
                    let mut ks = String::new();
                    push_escaped(&mut ks, k);
                    format!("{ks}:{}", dump(&map[*k]))
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
    }
}

/// nlohmann's `type_error` 316 for a string that is not valid UTF-8, or
/// `None` for a valid one. Its serialiser runs a byte-at-a-time decoder
/// over the string: a byte that cannot continue the current sequence is
/// reported by its index ("invalid UTF-8 byte at index 3: 0x41"), and a
/// string that ends inside a sequence by its last byte ("incomplete UTF-8
/// string; last byte: 0xF0"). `std::str::from_utf8` reports the start of
/// the ill-formed sequence and the length of its maximal valid prefix
/// instead, so the index is translated: a legal lead byte followed by a
/// wrong byte fails at the wrong byte; an illegal lead byte fails at
/// itself. `nix-instantiate --json` prints the text as is; `builtins.toJSON`
/// prefixes it (see `prim_to_json`).
pub fn utf8_error(s: &[u8]) -> Option<String> {
    let e = std::str::from_utf8(s).err()?;
    let start = e.valid_up_to();
    Some(match e.error_len() {
        None => format!(
            "[json.exception.type_error.316] incomplete UTF-8 string; last byte: 0x{:02X}",
            s[s.len() - 1]
        ),
        Some(prefix) => {
            let legal_lead = (0xC2..=0xF4).contains(&s[start]);
            let i = if legal_lead { start + prefix } else { start };
            format!(
                "[json.exception.type_error.316] invalid UTF-8 byte at index {i}: 0x{:02X}",
                s[i]
            )
        }
    })
}

/// The serialiser's string writer. nlohmann validates UTF-8 only when the
/// finished document is dumped, after the whole value has been forced, so
/// an invalid string must not interrupt the traversal: it is written
/// lossily and its error (the first one in document order) is remembered
/// for `to_json` to raise once everything else has succeeded.
fn push_json_string(out: &mut String, s: &[u8], first_err: &mut Option<String>) {
    if first_err.is_none() {
        *first_err = utf8_error(s);
    }
    push_escaped(out, &String::from_utf8_lossy(s));
}

/// A string as nlohmann writes it: quoted, with `"`, `\` and control
/// characters escaped.
pub(crate) fn push_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl<'a> Evaluator<'a> {
    /// `printValueAsJSON` and nlohmann's `dump()`. With `strict`, thunks
    /// are forced; with `copy_to_store`, paths become store paths (and
    /// their context is collected). A string that is not UTF-8 fails the
    /// dump with nlohmann's exception, which is not a Nix error
    /// ([`ErrorKind::Foreign`]).
    pub fn to_json(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        copy_to_store: bool,
    ) -> Result<String> {
        let (out, utf8_error) = self.to_json_undumped(cell, pos, context, strict, copy_to_store)?;
        match utf8_error {
            Some(msg) => Err(crate::error::EvalError::new(ErrorKind::Foreign, msg)),
            None => Ok(out),
        }
    }

    /// [`Evaluator::to_json`] for a caller that dumps the JSON later, as
    /// `derivationStrict` does with structured attributes: the text, with
    /// strings that are not UTF-8 written lossily, and nlohmann's error for
    /// the first of them, which the dump would raise.
    pub(crate) fn to_json_undumped(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        copy_to_store: bool,
    ) -> Result<(String, Option<String>)> {
        // With the parallel evaluator enabled, Nix deep-forces the value
        // before serialising it, so evaluation errors surface without
        // serialisation traces.
        if strict && self.config.eval_cores != 1 {
            self.force_deep_quiet(cell, pos)?;
        }
        let mut out = String::new();
        let mut first_err = None;
        self.to_json_inner(
            &mut out,
            cell,
            pos,
            context,
            strict,
            copy_to_store,
            &mut first_err,
        )?;
        // The dump happens after the traversal, as in Nix, so an invalid
        // string is reported only if nothing else failed.
        Ok((out, first_err))
    }

    /// One level of the serialisation: recurses as deep as the value
    /// nests, until `max-call-depth`, growing the stack as needed.
    #[allow(clippy::too_many_arguments)]
    fn to_json_inner(
        &self,
        out: &mut String,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        copy_to_store: bool,
        first_err: &mut Option<String>,
    ) -> Result<()> {
        self.with_stack(|| {
            self.to_json_level(out, cell, pos, context, strict, copy_to_store, first_err)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn to_json_level(
        &self,
        out: &mut String,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        copy_to_store: bool,
        first_err: &mut Option<String>,
    ) -> Result<()> {
        let _guard = self.enter(pos)?;
        let v = if strict {
            self.force(cell, pos)?
        } else {
            cell.get()
        };
        match v {
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            Value::String(s) => {
                let (bytes, string_context) = self.str_parts(s)?;
                context.extend(string_context.iter().copied());
                push_json_string(out, bytes, first_err);
            }
            Value::Path(p) => {
                if copy_to_store {
                    let sp = self.copy_path_to_store(context, p, self.determine_pos(v, pos))?;
                    push_json_string(
                        out,
                        self.config.store_dir.display(&sp).as_bytes(),
                        first_err,
                    );
                } else {
                    push_json_string(out, &self.path_text(p)?, first_err);
                }
            }
            Value::Null => out.push_str("null"),
            Value::Attrs(b) => {
                let mut tmp_ctx = StringContext::new();
                if let Some(s) = self.try_to_string_attr(pos, cell, &mut tmp_ctx)? {
                    context.extend_from(&tmp_ctx);
                    push_json_string(out, s, first_err);
                    return Ok(());
                }
                if let Some(a) = b.get(sym::OUT_PATH) {
                    return self.to_json_inner(
                        out,
                        a.value,
                        a.pos,
                        context,
                        strict,
                        copy_to_store,
                        first_err,
                    );
                }
                out.push('{');
                let mut first = true;
                for attr in self.lexicographic_attrs(b) {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    push_json_string(out, self.symbol_name(attr.name), first_err);
                    out.push(':');
                    if let Err(e) = self.to_json_inner(
                        out,
                        attr.value,
                        attr.pos,
                        context,
                        strict,
                        copy_to_store,
                        first_err,
                    ) {
                        self.add_trace(
                            &e,
                            attr.pos,
                            msg!(
                                "while evaluating attribute '",
                                self.symbol_ident(attr.name),
                                "'"
                            ),
                        );
                        return Err(e);
                    }
                }
                out.push('}');
            }
            Value::List(l) => {
                out.push('[');
                for (i, &elem) in l.elems.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Err(e) = self.to_json_inner(
                        out,
                        elem,
                        pos,
                        context,
                        strict,
                        copy_to_store,
                        first_err,
                    ) {
                        self.add_trace(
                            &e,
                            pos,
                            format!("while evaluating list element at index {i}"),
                        );
                        return Err(e);
                    }
                }
                out.push(']');
            }
            Value::Float(f) => out.push_str(&format_json_float(f)),
            _ => {
                return Err(self.error_at(
                    ErrorKind::TypeError,
                    format!("cannot convert {} to JSON", v.show_type()),
                    self.determine_pos(v, pos),
                ))
            }
        }
        Ok(())
    }

    /// `tryAttrsToString` with `coerceMore = false, copyToStore = false`.
    fn try_to_string_attr(
        &self,
        pos: SrcPos,
        cell: ValueRef<'a>,
        context: &mut StringContext<'a>,
    ) -> Result<Option<&'a [u8]>> {
        let Value::Attrs(b) = cell.get() else {
            return Ok(None);
        };
        let Some(ts) = b.get(sym::TO_STRING) else {
            return Ok(None);
        };
        let f = self.force(ts.value, pos)?;
        let r = self.call(f, &[cell], pos)?;
        let rc = self.alloc_cell(r);
        Ok(Some(self.coerce_to_string(
            pos,
            rc,
            context,
            "while evaluating the result of the `__toString` attribute",
            Coercion::NO_COPY,
        )?))
    }

    /// `parseJSON`: a JSON document to a value, read by nlohmann's parser
    /// ([`brix_store::json::sax_parse`]) the way Nix's `JSONSax` takes it:
    /// a key given twice keeps its last value, and the values Nix refuses
    /// (an unsigned number above `i64::MAX`, a string holding a NUL byte)
    /// fail where they are read, before any syntax error further on. A
    /// syntax error, or a number too large for a double, is nlohmann's
    /// exception, an [`ErrorKind::Parse`] error (`JSONParseError`); the
    /// values Nix refuses are plain errors. Nesting is not limited: the
    /// parse keeps the open arrays and objects on a heap stack.
    pub fn from_json(&self, text: &[u8]) -> Result<Value<'a>> {
        let mut builder = JsonBuilder {
            ev: self,
            open: Vec::new(),
            root: None,
        };
        match brix_store::json::sax_parse(text, &mut builder) {
            Ok(()) => Ok(builder.root.expect("a document that parses holds a value")),
            Err(SaxError::Parse(e)) => Err(self.error(ErrorKind::Parse, e.message().to_vec())),
            Err(SaxError::Handler(e)) => Err(e),
        }
    }
}

/// Nix's `JSONSax`: the value of a JSON document, built bottom-up as
/// nlohmann's parser reports it.
struct JsonBuilder<'e, 'a> {
    ev: &'e Evaluator<'a>,
    /// The arrays and objects being read, innermost last.
    open: Vec<JsonOpen<'a>>,
    root: Option<Value<'a>>,
}

enum JsonOpen<'a> {
    List(Vec<ValueRef<'a>>),
    /// An object's attributes, and the name whose value comes next.
    Attrs(Vec<Attr<'a>>, Option<Symbol>),
}

impl<'a> JsonBuilder<'_, 'a> {
    /// A value read: an element, an attribute's value, or the document.
    fn add(&mut self, value: Value<'a>) {
        match self.open.last_mut() {
            Some(JsonOpen::List(items)) => items.push(self.ev.alloc_cell(value)),
            Some(JsonOpen::Attrs(attrs, name)) => {
                let name = name.take().expect("a value in an object follows its key");
                attrs.push(self.ev.attr(name, value));
            }
            None => self.root = Some(value),
        }
    }

    /// `forceNoNullByte`: a Nix string cannot hold a NUL byte.
    fn no_null_byte(&self, s: &[u8]) -> Result<()> {
        if !s.contains(&0) {
            return Ok(());
        }
        let shown = std::str::from_utf8(s)
            .expect("nlohmann's lexer only lets UTF-8 through")
            .replace('\0', "␀");
        Err(self.ev.error(
            ErrorKind::Eval,
            format!(
                "input string '{shown}' cannot be represented as Nix string because it contains null bytes"
            ),
        ))
    }
}

impl<'a> Sax for JsonBuilder<'_, 'a> {
    type Error = crate::Error;

    fn null(&mut self) -> Result<()> {
        self.add(Value::Null);
        Ok(())
    }

    fn boolean(&mut self, value: bool) -> Result<()> {
        self.add(Value::Bool(value));
        Ok(())
    }

    fn number_integer(&mut self, value: i64) -> Result<()> {
        self.add(Value::Int(value));
        Ok(())
    }

    fn number_unsigned(&mut self, value: u64) -> Result<()> {
        let Ok(value) = i64::try_from(value) else {
            return Err(self.ev.error(
                ErrorKind::Eval,
                format!("unsigned json number {value} outside of Nix integer range"),
            ));
        };
        self.add(Value::Int(value));
        Ok(())
    }

    fn number_float(&mut self, value: f64, _text: &[u8]) -> Result<()> {
        self.add(Value::Float(value));
        Ok(())
    }

    fn string(&mut self, value: &[u8]) -> Result<()> {
        self.no_null_byte(value)?;
        let string = self.ev.alloc_string_value(value, &[]);
        self.add(string);
        Ok(())
    }

    fn start_object(&mut self) -> Result<()> {
        self.open.push(JsonOpen::Attrs(Vec::new(), None));
        Ok(())
    }

    fn key(&mut self, key: &[u8]) -> Result<()> {
        self.no_null_byte(key)?;
        let name = self.ev.intern(key);
        if let Some(JsonOpen::Attrs(_, pending)) = self.open.last_mut() {
            *pending = Some(name);
        }
        Ok(())
    }

    fn end_object(&mut self) -> Result<()> {
        if let Some(JsonOpen::Attrs(attrs, _)) = self.open.pop() {
            let set = self.ev.mk_attrs(attrs, SrcPos::NONE);
            self.add(set);
        }
        Ok(())
    }

    fn start_array(&mut self) -> Result<()> {
        self.open.push(JsonOpen::List(Vec::new()));
        Ok(())
    }

    fn end_array(&mut self) -> Result<()> {
        if let Some(JsonOpen::List(items)) = self.open.pop() {
            let list = self.ev.mk_list(&items);
            self.add(list);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_floats_match_nlohmann() {
        let cases = [
            (0.1, "0.1"),
            (1e30, "1e+30"),
            (100000.0, "100000.0"),
            (2.0, "2.0"),
            (1.0 / 3.0, "0.3333333333333333"),
            (1e-7, "1e-07"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (1e21, "1e+21"),
            (1e15, "1e+15"),
            (1e14, "100000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (12345678.9, "12345678.9"),
            (9007199254740993.0, "9.007199254740992e+15"),
            (123.456, "123.456"),
            (0.001, "0.001"),
            (12345678901234567890.0, "1.2345678901234567e+19"),
            (-1.5, "-1.5"),
            (f64::INFINITY, "null"),
            (f64::NAN, "null"),
        ];
        for (f, expected) in cases {
            assert_eq!(format_json_float(f), expected, "{f}");
        }
    }

    #[test]
    fn json_strings() {
        let mut out = String::new();
        push_escaped(&mut out, "x\u{1}\t\"\\é</");
        assert_eq!(out, "\"x\\u0001\\t\\\"\\\\é</\"");
    }
}
