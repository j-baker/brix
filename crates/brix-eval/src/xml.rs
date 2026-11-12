//! `builtins.toXML` and `nix-instantiate --xml` (`value-to-xml.cc`).

use std::collections::BTreeSet;

use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{SrcPos, Value, ValueRef};
use crate::xml_writer::XmlWriter;
use crate::Result;
use brix_syntax::float::format_g;

impl<'a> Evaluator<'a> {
    /// `printValueAsXML`. The document holds strings' bytes as they are,
    /// so it is UTF-8 only if they are.
    pub fn to_xml(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        location: bool,
    ) -> Result<Vec<u8>> {
        let mut w = XmlWriter::new();
        w.open_element("expr", &[]);
        let mut drvs_seen = BTreeSet::new();
        self.to_xml_inner(&mut w, cell, pos, context, strict, location, &mut drvs_seen)?;
        w.close_element();
        Ok(w.out)
    }

    fn pos_attrs(&self, pos: SrcPos) -> Result<Vec<(&'static str, Vec<u8>)>> {
        let mut attrs = Vec::new();
        if let Some(p) = self.position(pos) {
            if let crate::ir::Origin::File { path, .. } = &p.info.origin {
                attrs.push(("path", self.logical_path(path)?.into_owned()));
            }
            attrs.push(("line", p.line.to_string().into_bytes()));
            attrs.push(("column", p.column.to_string().into_bytes()));
        }
        Ok(attrs)
    }

    #[allow(clippy::too_many_arguments)]
    fn show_attrs_xml(
        &self,
        w: &mut XmlWriter,
        b: crate::value::Bindings<'a>,
        context: &mut StringContext<'a>,
        strict: bool,
        location: bool,
        drvs_seen: &mut BTreeSet<Vec<u8>>,
    ) -> Result<()> {
        for attr in self.lexicographic_attrs(b) {
            let mut xml_attrs: Vec<(&str, Vec<u8>)> =
                vec![("name", self.symbol_name(attr.name).to_vec())];
            if location && attr.pos.is_some() {
                xml_attrs.extend(self.pos_attrs(attr.pos)?);
            }
            let borrowed: Vec<(&str, &[u8])> =
                xml_attrs.iter().map(|(k, v)| (*k, v.as_slice())).collect();
            w.open_element("attr", &borrowed);
            self.to_xml_inner(
                w, attr.value, attr.pos, context, strict, location, drvs_seen,
            )?;
            w.close_element();
        }
        Ok(())
    }

    /// One level of the document: recurses as deep as the value nests,
    /// until `max-call-depth`, growing the stack as needed.
    #[allow(clippy::too_many_arguments)]
    fn to_xml_inner(
        &self,
        w: &mut XmlWriter,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        location: bool,
        drvs_seen: &mut BTreeSet<Vec<u8>>,
    ) -> Result<()> {
        self.with_stack(|| self.to_xml_level(w, cell, pos, context, strict, location, drvs_seen))
    }

    #[allow(clippy::too_many_arguments)]
    fn to_xml_level(
        &self,
        w: &mut XmlWriter,
        cell: ValueRef<'a>,
        pos: SrcPos,
        context: &mut StringContext<'a>,
        strict: bool,
        location: bool,
        drvs_seen: &mut BTreeSet<Vec<u8>>,
    ) -> Result<()> {
        let _guard = self.enter(pos)?;
        let v = if strict {
            self.force(cell, pos)?
        } else {
            cell.get()
        };
        match v {
            Value::Int(i) => w.empty_element("int", &[("value", i.to_string().as_bytes())]),
            Value::Bool(b) => {
                w.empty_element("bool", &[("value", if b { b"true" } else { b"false" })])
            }
            Value::String(s) => {
                let (bytes, string_context) = self.str_parts(s)?;
                context.extend(string_context.iter().copied());
                w.empty_element("string", &[("value", bytes)]);
            }
            Value::Path(p) => {
                let shown = self.show_path(p);
                w.empty_element("path", &[("value", shown.as_slice())]);
            }
            Value::Null => w.empty_element("null", &[]),
            Value::Attrs(b) => {
                if self.is_derivation(v)? {
                    let mut xml_attrs: Vec<(&str, Vec<u8>)> = Vec::new();
                    let mut drv_path: Vec<u8> = Vec::new();
                    if let Some(a) = b.get(sym::DRV_PATH) {
                        let av = if strict {
                            self.force(a.value, a.pos)?
                        } else {
                            a.value.get()
                        };
                        if let Value::String(s) = av {
                            drv_path = self.str_bytes(s)?.to_vec();
                            xml_attrs.push(("drvPath", drv_path.clone()));
                        }
                    }
                    if let Some(a) = b.get(sym::OUT_PATH) {
                        let av = if strict {
                            self.force(a.value, a.pos)?
                        } else {
                            a.value.get()
                        };
                        if let Value::String(s) = av {
                            xml_attrs.push(("outPath", self.str_bytes(s)?.to_vec()));
                        }
                    }
                    let borrowed: Vec<(&str, &[u8])> =
                        xml_attrs.iter().map(|(k, v)| (*k, v.as_slice())).collect();
                    w.open_element("derivation", &borrowed);
                    if !drv_path.is_empty() && drvs_seen.insert(drv_path) {
                        self.show_attrs_xml(w, b, context, strict, location, drvs_seen)?;
                    } else {
                        w.empty_element("repeated", &[]);
                    }
                    w.close_element();
                } else {
                    w.open_element("attrs", &[]);
                    self.show_attrs_xml(w, b, context, strict, location, drvs_seen)?;
                    w.close_element();
                }
            }
            Value::List(l) => {
                w.open_element("list", &[]);
                for &elem in l.elems {
                    self.to_xml_inner(w, elem, pos, context, strict, location, drvs_seen)?;
                }
                w.close_element();
            }
            Value::Lambda { node, .. } => {
                let (code, lam) = self.lambda_def(node);
                let mut xml_attrs: Vec<(&str, Vec<u8>)> = Vec::new();
                if location {
                    xml_attrs.extend(self.pos_attrs(self.src_pos(code, lam.pos))?);
                }
                let borrowed: Vec<(&str, &[u8])> =
                    xml_attrs.iter().map(|(k, v)| (*k, v.as_slice())).collect();
                w.open_element("function", &borrowed);
                match &lam.formals {
                    Some(formals) => {
                        let mut pat: Vec<(&str, Vec<u8>)> = Vec::new();
                        if let Some(a) = lam.arg {
                            pat.push(("name", self.symbol_name(a).to_vec()));
                        }
                        if formals.ellipsis {
                            pat.push(("ellipsis", b"1".to_vec()));
                        }
                        let borrowed: Vec<(&str, &[u8])> =
                            pat.iter().map(|(k, v)| (*k, v.as_slice())).collect();
                        w.open_element("attrspat", &borrowed);
                        let mut fs: Vec<&crate::ir::Formal> = formals.formals.iter().collect();
                        fs.sort_by(|a, b| self.symbol_name(a.name).cmp(self.symbol_name(b.name)));
                        for f in fs {
                            w.empty_element("attr", &[("name", self.symbol_name(f.name))]);
                        }
                        w.close_element();
                    }
                    None => {
                        w.empty_element(
                            "varpat",
                            &[(
                                "name",
                                self.symbol_name(lam.arg.expect("plain lambda has an argument")),
                            )],
                        );
                    }
                }
                w.close_element();
            }
            Value::PrimOp(_) | Value::PrimOpApp(_) => w.empty_element("unevaluated", &[]),
            Value::Float(f) => w.empty_element("float", &[("value", format_g(f).as_bytes())]),
            Value::Thunk { .. } | Value::App(_) | Value::Blackhole | Value::Failed(_) => {
                w.empty_element("unevaluated", &[])
            }
        }
        Ok(())
    }
}
