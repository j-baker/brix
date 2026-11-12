//! Owned snapshots of values, for callers that outlive the evaluator.

use std::collections::BTreeMap;

use bstr::BString;

use crate::evaluator::Evaluator;
use crate::value::{ContextElem, Value, ValueRef};
use crate::Result;

/// A fully forced value copied out of the arena.
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String {
        bytes: Vec<u8>,
        /// Encoded context elements (`ContextElem::encode`): bytes, as
        /// an output name in them is.
        context: Vec<Vec<u8>>,
    },
    /// A path's text (a lazy tree's resolved): bytes, as Nix's paths are.
    Path(BString),
    /// By attribute name: bytes, as Nix's names are.
    Attrs(BTreeMap<Vec<u8>, OwnedValue>),
    List(Vec<OwnedValue>),
    /// Functions have no owned form; only their description is kept, as
    /// Nix prints it in messages (bytes, quoting what the function names).
    Function(Vec<u8>),
}

impl<'a> Evaluator<'a> {
    /// Forces `cell` deeply and copies it out.
    pub fn to_owned_value(&self, cell: ValueRef<'a>) -> Result<OwnedValue> {
        self.force_deep(cell)?;
        self.owned_inner(cell.get())
    }

    fn owned_inner(&self, v: Value<'a>) -> Result<OwnedValue> {
        Ok(match v {
            Value::Int(i) => OwnedValue::Int(i),
            Value::Float(f) => OwnedValue::Float(f),
            Value::Bool(b) => OwnedValue::Bool(b),
            Value::Null => OwnedValue::Null,
            Value::String(s) => {
                let (bytes, context) = self.str_parts_resolved(s)?;
                OwnedValue::String {
                    bytes: bytes.to_vec(),
                    context: context
                        .iter()
                        .map(|c: &ContextElem<'a>| c.encode())
                        .collect(),
                }
            }
            Value::Path(p) => OwnedValue::Path(self.path_text(p)?.into_owned().into()),
            Value::Attrs(b) => OwnedValue::Attrs(
                b.iter()
                    .map(|a| {
                        Ok((
                            self.symbol_name(a.name).to_vec(),
                            self.owned_inner(a.value.get())?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            Value::List(l) => OwnedValue::List(
                l.elems
                    .iter()
                    .map(|e| self.owned_inner(e.get()))
                    .collect::<Result<_>>()?,
            ),
            // A function prints as its name and position.
            other => OwnedValue::Function(self.print_for_error(other).to_vec()),
        })
    }
}
