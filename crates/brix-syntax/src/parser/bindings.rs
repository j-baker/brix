//! Attribute-set construction: nested attribute paths and the duplicate
//! checks (`addAttr` in `parser-state.hh`).

use hashbrown::HashMap;

use crate::ast::{AttrDef, AttrName, AttrSet, AttrValue, DynamicAttrDef, Expr, Name, Pos};
use crate::error::ParseError;
use crate::printer::show_attr_path;

/// Name → index into `AttrSet::attrs` for the set being parsed. Nix keeps
/// its bindings in a map; the AST keeps a vector, so this index makes the
/// duplicate check constant time for large sets such as `all-packages.nix`.
/// Nested sets are small and searched linearly.
/// `hashbrown` (foldhash) rather than `std`'s SipHash: every attribute of
/// every set goes through this index, and SipHash was 5% of parse time.
pub(crate) type AttrIndex = HashMap<Name, usize>;

/// Adds `path = value;` to `attrs`, creating intermediate (non-recursive)
/// sets for `a.b.c = …` and merging into sets that already exist.
pub(crate) fn add_attr(
    attrs: &mut AttrSet,
    index: &mut AttrIndex,
    mut path: Vec<AttrName>,
    pos: Pos,
    value: Expr,
) -> Result<(), ParseError> {
    let last = path
        .pop()
        .expect("an attribute path has at least one component");
    let mut cur = attrs;
    let mut cur_index = Some(index);
    for i in 0..path.len() {
        match &path[i] {
            AttrName::Static(name) => {
                let name = name.clone();
                let existing = match &cur_index {
                    Some(index) => index.get(&name).copied(),
                    None => cur.attrs.iter().position(|d| d.name == name),
                };
                if let Some(j) = existing {
                    let prev_pos = cur.attrs[j].pos;
                    match &mut cur.attrs[j].value {
                        AttrValue::Plain(e) if matches!(**e, Expr::Attrs(_)) => {
                            let Expr::Attrs(nested) = &mut **e else {
                                unreachable!()
                            };
                            cur = nested;
                        }
                        _ => {
                            path.truncate(i + 1);
                            return Err(dup_attr(&path, pos, prev_pos));
                        }
                    }
                } else {
                    if let Some(index) = &mut cur_index {
                        index.insert(name.clone(), cur.attrs.len());
                    }
                    cur.attrs.push(AttrDef {
                        name,
                        pos,
                        value: AttrValue::Plain(Box::new(Expr::Attrs(Box::default()))),
                    });
                    let AttrValue::Plain(e) = &mut cur.attrs.last_mut().expect("just pushed").value
                    else {
                        unreachable!()
                    };
                    let Expr::Attrs(nested) = &mut **e else {
                        unreachable!()
                    };
                    cur = nested;
                }
            }
            AttrName::Dynamic(name_expr) => {
                cur.dynamic_attrs.push(DynamicAttrDef {
                    name: name_expr.clone(),
                    value: Box::new(Expr::Attrs(Box::default())),
                    pos,
                });
                let Expr::Attrs(nested) =
                    &mut *cur.dynamic_attrs.last_mut().expect("just pushed").value
                else {
                    unreachable!()
                };
                cur = nested;
            }
        }
        cur_index = None;
    }
    match last {
        AttrName::Static(name) => {
            path.push(AttrName::Static(name.clone()));
            add_attr_leaf(
                cur,
                cur_index,
                &mut path,
                AttrDef {
                    name,
                    pos,
                    value: AttrValue::Plain(Box::new(value)),
                },
            )
        }
        AttrName::Dynamic(name_expr) => {
            cur.dynamic_attrs.push(DynamicAttrDef {
                name: name_expr,
                value: Box::new(value),
                pos,
            });
            Ok(())
        }
    }
}

/// Inserts a definition whose name is the last component of `path`. If an
/// attribute set is being defined where one already exists, the two are
/// merged (recursively) instead; any other clash is an error. As in Nix, a
/// `rec` on the new set is lost in a merge and the existing set's `rec`
/// applies to everything.
fn add_attr_leaf(
    attrs: &mut AttrSet,
    index: Option<&mut AttrIndex>,
    path: &mut Vec<AttrName>,
    def: AttrDef,
) -> Result<(), ParseError> {
    let existing = match &index {
        Some(index) => index.get(&def.name).copied(),
        None => attrs.attrs.iter().position(|d| d.name == def.name),
    };
    let Some(j) = existing else {
        if let Some(index) = index {
            index.insert(def.name.clone(), attrs.attrs.len());
        }
        attrs.attrs.push(def);
        return Ok(());
    };
    let prev_pos = attrs.attrs[j].pos;
    let AttrValue::Plain(existing) = &mut attrs.attrs[j].value else {
        return Err(dup_attr(path, def.pos, prev_pos));
    };
    let Expr::Attrs(existing) = &mut **existing else {
        return Err(dup_attr(path, def.pos, prev_pos));
    };
    let AttrValue::Plain(mut new) = def.value else {
        return Err(dup_attr(path, def.pos, prev_pos));
    };
    let Expr::Attrs(new) = &mut *new else {
        return Err(dup_attr(path, def.pos, prev_pos));
    };
    // Taken rather than moved out: `Expr` has a destructor.
    let new = std::mem::take(&mut **new);
    let base = existing.inherit_from.len() as u32;
    for mut ad in new.attrs {
        if let AttrValue::InheritedFrom { index } = &mut ad.value {
            *index += base;
        }
        path.push(AttrName::Static(ad.name.clone()));
        add_attr_leaf(existing, None, path, ad)?;
        path.pop();
    }
    existing.dynamic_attrs.extend(new.dynamic_attrs);
    existing.inherit_from.extend(new.inherit_from);
    Ok(())
}

/// `dupAttr`: "attribute 'a.b' already defined at «stdin»:1:3", with the
/// earlier position kept apart (see [`ParseError::defined_at`]).
fn dup_attr(path: &[AttrName], pos: Pos, prev_pos: Pos) -> ParseError {
    ParseError {
        msg: [
            &b"attribute '"[..],
            &show_attr_path(path),
            b"' already defined",
        ]
        .concat()
        .into(),
        pos: Some(pos),
        defined_at: Some(prev_pos),
    }
}

/// The same check for `inherit`, where the name is a single symbol.
pub(crate) fn dup_inherit(name: &Name, pos: Pos, prev_pos: Pos) -> ParseError {
    dup_attr(&[AttrName::Static(name.clone())], pos, prev_pos)
}
