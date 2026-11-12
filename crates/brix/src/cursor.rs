//! Lazy access to evaluated values: Nix's `AttrCursor`, without the
//! on-disk cache. A [`Cursor`] names a value the session keeps alive and
//! the attribute path it was reached by, which is what the error
//! messages (`attribute 'packages.x86_64-linux.foo' does not exist`,
//! `'…' is not a string`) and the CLI's attribute-path logic need.

use brix_eval::managed_api::{GcContext, GcValue, GcValueView};
use brix_eval::value::{SrcPos, ValueType};
use brix_eval::OwnedValue;
use brix_store::StorePath;

use crate::error::{Error, Result};
use crate::session::{Ctx, Format, Session};
use crate::value_slots::Lease;

/// A reference to a (possibly unevaluated) value.
#[derive(Clone)]
pub struct Cursor<'s> {
    pub(crate) session: &'s Session,
    pub(crate) slot: Lease,
    pub(crate) attr_path: Vec<String>,
    /// Where the value was defined, when it was reached as an attribute
    /// (the attribute's position, as `findAlongAttrPath` returns it);
    /// `SrcPos::NONE` otherwise.
    pub(crate) pos: SrcPos,
}

impl std::fmt::Debug for Cursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor({})", self.attr_path_str())
    }
}

/// `AttrPath::to_string`: components joined by `.`, each quoted unless it
/// is a plain identifier (`printAttributeName`).
pub fn attr_path_to_string(path: &[String]) -> String {
    let mut out = Vec::new();
    for (i, name) in path.iter().enumerate() {
        if i > 0 {
            out.push(b'.');
        }
        brix_eval::print_attribute_name(&mut out, name.as_bytes());
    }
    String::from_utf8(out).expect("names that are text print as text")
}

/// `parseAttrPath`: dot-separated components, with `"…"` quoting a
/// component that contains dots.
pub fn parse_attr_path(path: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = path.chars().peekable();
    if path.is_empty() {
        return Ok(parts);
    }
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                parts.push(std::mem::take(&mut cur));
            }
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) => cur.push(c),
                    None => {
                        return Err(Error::Usage(
                            format!("missing closing quote in selection path '{path}'").into(),
                        ))
                    }
                }
            },
            c => cur.push(c),
        }
    }
    parts.push(cur);
    Ok(parts)
}

/// One element of a string's context (`NixStringContextElem`): what the
/// string refers to in the store.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextElem {
    /// A store path (encoded as the path itself).
    Opaque(StorePath),
    /// A derivation and its whole build closure (`=<drv>`).
    DrvDeep(StorePath),
    /// One output of a derivation (`!<output>!<drv>`). The output name is
    /// bytes, as Nix's are: a floating output's need not be UTF-8.
    Built {
        drv: StorePath,
        output: bstr::BString,
    },
    /// A store path mentioned without being copied (Determinate Nix's
    /// `Path`, `@<path>`); it does not count as context.
    Path(StorePath),
}

impl ContextElem {
    /// The owned form of the evaluator's elements. Lazy elements are
    /// resolved by the evaluator before a context leaves it.
    fn all(elems: impl IntoIterator<Item = GcContext>) -> Result<Vec<ContextElem>> {
        let path = |p: Vec<u8>| StorePath::new(p).map_err(Error::from);
        elems
            .into_iter()
            .map(|elem| {
                Ok(match elem {
                    GcContext::Opaque(p) => ContextElem::Opaque(path(p)?),
                    GcContext::DrvDeep(p) => ContextElem::DrvDeep(path(p)?),
                    GcContext::Built { drv, output } => ContextElem::Built {
                        drv: path(drv)?,
                        output: output.into(),
                    },
                    GcContext::Path(p) => ContextElem::Path(path(p)?),
                })
            })
            .collect()
    }
}

impl<'s> Cursor<'s> {
    pub(crate) fn new(session: &'s Session, slot: Lease, attr_path: Vec<String>) -> Cursor<'s> {
        Cursor {
            session,
            slot,
            attr_path,
            pos: SrcPos::NONE,
        }
    }

    /// The cursor with `pos` as where its value was defined.
    pub(crate) fn with_pos(mut self, pos: SrcPos) -> Cursor<'s> {
        self.pos = pos;
        self
    }

    /// The same value under another attribute path (the inventory's
    /// `children` nesting is reported as the output's own attribute path).
    pub fn renamed(&self, attr_path: Vec<String>) -> Cursor<'s> {
        Cursor::new(self.session, self.slot.clone(), attr_path)
    }

    /// The session the value lives in.
    pub fn session(&self) -> &'s Session {
        self.session
    }

    pub fn attr_path(&self) -> &[String] {
        &self.attr_path
    }

    /// The attribute path as Nix prints it (`getAttrPathStr`).
    pub fn attr_path_str(&self) -> String {
        attr_path_to_string(&self.attr_path)
    }

    fn attr_path_str_with(&self, name: &str) -> String {
        let mut p = self.attr_path.clone();
        p.push(name.to_owned());
        attr_path_to_string(&p)
    }

    pub(crate) fn with<R>(
        &self,
        f: impl for<'a> FnOnce(&mut Ctx<'_, 'a>, GcValue) -> Result<R>,
    ) -> Result<R> {
        self.session.with_slot(&self.slot, f)
    }

    fn eval_err(&self, ctx: &Ctx<'_, '_>, e: brix_eval::Error) -> Error {
        ctx.host
            .eval_error(&e, self.session.config().eval.show_trace)
    }

    /// [`Session::error_with_trace`] with the trace at where the value was
    /// defined (the attribute's position, when it was reached as one).
    pub fn error_with_trace(&self, msg: impl Into<bstr::BString>, trace: &str) -> Error {
        self.session.traced_error(msg, self.pos, trace)
    }

    /// Forces the value and reports its type.
    pub fn value_type(&self) -> Result<ValueType> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            ctx.ev.value_type(&cell).map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// The forced value as Nix shows it in an error message
    /// (`ValuePrinter` with `errorPrintOptions`): what is not evaluated
    /// yet inside it shows as `«thunk»`, nothing is forced. Bytes, as Nix
    /// prints them: a string in it need not be UTF-8.
    pub fn print_for_error(&self) -> Result<Vec<u8>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            ctx.ev
                .print_for_error(&cell)
                .map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// `showType` of the forced value ("a set", "a string", …).
    pub fn show_type(&self) -> Result<String> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            ctx.ev.show_type(&cell).map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// The attribute `name`, or `None` when the value is not a set or
    /// lacks it (`AttrCursor::maybeGetAttr`).
    pub fn maybe_get_attr(&self, name: &str) -> Result<Option<Cursor<'s>>> {
        let slot = self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(None);
            };
            Ok(attrs.get_with_pos(name.as_bytes()))
        })?;
        Ok(slot.map(|(slot, pos)| {
            let mut path = self.attr_path.clone();
            path.push(name.to_owned());
            Cursor::new(self.session, slot, path).with_pos(pos)
        }))
    }

    /// The attribute `name`, with Nix's error when it is missing.
    pub fn get_attr(&self, name: &str) -> Result<Cursor<'s>> {
        match self.maybe_get_attr(name)? {
            Some(c) => Ok(c),
            None => Err(Error::Usage(
                format!(
                    "attribute '{}' does not exist",
                    self.attr_path_str_with(name)
                )
                .into(),
            )),
        }
    }

    /// The attribute names of a set, sorted (`AttrCursor::getAttrs`).
    /// They are text, as the attribute paths the caller goes on to use are;
    /// a name that is not UTF-8 is refused rather than altered.
    pub fn get_attrs(&self) -> Result<Vec<String>> {
        let mut names = self.get_attrs_in_set_order()?;
        names.sort();
        Ok(names)
    }

    /// The attribute names of a set in `Bindings` (symbol id) order, the
    /// order in which Nix walks a set itself (`nix eval --write-to`), as
    /// text like [`Cursor::get_attrs`]'s.
    pub fn get_attrs_in_set_order(&self) -> Result<Vec<String>> {
        self.with(|ctx, cell| {
            ctx.ev
                .force_attrs(&cell, SrcPos::NONE, "")
                .map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                unreachable!("forced attrs")
            };
            attrs
                .names()
                .map(|name| {
                    std::str::from_utf8(name)
                        .map(str::to_owned)
                        .map_err(|_| self.not_utf8("an attribute name", name))
                })
                .collect()
        })
    }

    /// `AttrCursor::getString`: the value must be a string (a path is
    /// not accepted here, unlike `coerceToString`).
    pub fn get_string(&self) -> Result<String> {
        Ok(self.get_string_with_context()?.0)
    }

    /// The string, as text, and its context: a string that is not UTF-8
    /// is refused rather than altered ([`Cursor::get_bytes_with_context`]
    /// has its bytes).
    pub fn get_string_with_context(&self) -> Result<(String, Vec<ContextElem>)> {
        let (bytes, context) = self.get_bytes_with_context()?;
        let text = String::from_utf8(bytes).map_err(|e| self.not_utf8("a string", e.as_bytes()))?;
        Ok((text, context))
    }

    /// The error for text the value holds as bytes that are not UTF-8,
    /// where the caller can only take text.
    fn not_utf8(&self, what: &str, bytes: &[u8]) -> Error {
        use bstr::ByteSlice;
        Error::Usage(
            format!(
                "{what} at '{}' is not valid UTF-8 ('{}'), which brix does not support here",
                self.attr_path_str(),
                bytes.to_str_lossy()
            )
            .into(),
        )
    }

    /// The string's bytes, which need not be UTF-8, and its context.
    pub fn get_bytes_with_context(&self) -> Result<(Vec<u8>, Vec<ContextElem>)> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            if !matches!(
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?,
                GcValueView::String(_)
            ) {
                let kind = ctx.ev.show_type(&cell).map_err(|e| self.eval_err(ctx, e))?;
                return Err(Error::Usage(
                    format!("'{}' is not a string but {}", self.attr_path_str(), kind).into(),
                ));
            }
            let (bytes, context) = ctx
                .ev
                .string_parts(&cell)
                .map_err(|e| self.eval_err(ctx, e))?;
            Ok((bytes, ContextElem::all(context)?))
        })
    }

    /// `fetchToStore(…, FetchMode::Copy)` of the path value, with Nix's
    /// default name `source`: the value "added" to the (virtual) store as
    /// a NAR, as a path installable is (`trySinglePathToDerivedPaths`).
    /// `None` if the value is not a path.
    pub fn fetch_to_store(&self) -> Result<Option<StorePath>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Path { accessor, bytes } =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(None);
            };
            let source = brix_store::SourcePath::new(
                accessor,
                brix_store::CanonPath::new(bytes).expect("managed path is canonical"),
            );
            ctx.ev
                .services()
                .add_path_to_store(
                    &source,
                    "source",
                    brix_store::ContentAddressMethod::NixArchive,
                    None,
                    SrcPos::NONE,
                )
                .map(Some)
                .map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// `coerceToPath` of the value: where the path is (its accessor and
    /// the path in it) and how Nix shows it (`SourcePath::to_string`),
    /// bytes, as path names are. The path is not copied to the store.
    pub fn coerce_to_source_path(
        &self,
    ) -> Result<(
        std::sync::Arc<dyn brix_store::SourceAccessor>,
        brix_store::CanonPath,
        bstr::BString,
    )> {
        self.with(|ctx, cell| {
            let source = ctx
                .ev
                .coerce_to_source_path(&cell, SrcPos::NONE, "")
                .map_err(|e| self.eval_err(ctx, e))?;
            let accessor = ctx.ev.services().accessor(source.accessor);
            let shown = accessor.show_path(&source.path);
            Ok((accessor, source.path, shown))
        })
    }

    /// `coerceToString` of the value: its bytes, which need not be UTF-8
    /// (`nix eval --raw` writes them as they are), with its context.
    pub fn coerce_to_bytes(&self) -> Result<(Vec<u8>, Vec<ContextElem>)> {
        self.with(|ctx, cell| {
            let value = ctx
                .ev
                .coerce_with_context(
                    &cell,
                    SrcPos::NONE,
                    brix_eval::Coercion::COPY,
                    "while evaluating a path segment",
                )
                .map_err(|e| self.eval_err(ctx, e))?;
            let (bytes, context) = ctx
                .ev
                .string_parts(&value)
                .map_err(|e| self.eval_err(ctx, e))?;
            Ok((bytes, ContextElem::all(context)?))
        })
    }

    pub fn get_bool(&self) -> Result<bool> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            match ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))? {
                GcValueView::Bool(value) => Ok(value),
                _ => Err(Error::Usage(
                    format!("'{}' is not a Boolean", self.attr_path_str()).into(),
                )),
            }
        })
    }

    pub fn get_int(&self) -> Result<i64> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            match ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))? {
                GcValueView::Int(value) => Ok(value),
                _ => Err(Error::Usage(
                    format!("'{}' is not an integer", self.attr_path_str()).into(),
                )),
            }
        })
    }

    /// The list's strings as bytes, which need not be UTF-8 (a
    /// derivation's output names).
    pub fn get_list_of_bytes(&self) -> Result<Vec<Vec<u8>>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::List(list) = ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Err(Error::Usage(
                    format!("'{}' is not a list", self.attr_path_str()).into(),
                ));
            };
            let len = list.len();
            let mut result = Vec::with_capacity(len);
            for index in 0..len {
                let elem = ctx
                    .ev
                    .list_item(&cell, index)
                    .map_err(|e| self.eval_err(ctx, e))?
                    .expect("list length stays fixed");
                ctx.ev.force(&elem).map_err(|e| self.eval_err(ctx, e))?;
                if !matches!(
                    ctx.ev.view(&elem).map_err(|e| self.eval_err(ctx, e))?,
                    GcValueView::String(_)
                ) {
                    return Err(Error::Usage(
                        format!("'{}' is not a list of strings", self.attr_path_str()).into(),
                    ));
                }
                result.push(
                    ctx.ev
                        .string_bytes(&elem)
                        .map_err(|e| self.eval_err(ctx, e))?,
                );
            }
            Ok(result)
        })
    }

    pub fn get_list_of_strings(&self) -> Result<Vec<String>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::List(list) = ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Err(Error::Usage(
                    format!("'{}' is not a list", self.attr_path_str()).into(),
                ));
            };
            let len = list.len();
            let mut result = Vec::with_capacity(len);
            for index in 0..len {
                let elem = ctx
                    .ev
                    .list_item(&cell, index)
                    .map_err(|e| self.eval_err(ctx, e))?
                    .expect("list length stays fixed");
                ctx.ev.force(&elem).map_err(|e| self.eval_err(ctx, e))?;
                if !matches!(
                    ctx.ev.view(&elem).map_err(|e| self.eval_err(ctx, e))?,
                    GcValueView::String(_)
                ) {
                    return Err(Error::Usage(
                        format!("'{}' is not a list of strings", self.attr_path_str()).into(),
                    ));
                }
                let bytes = ctx
                    .ev
                    .string_bytes(&elem)
                    .map_err(|e| self.eval_err(ctx, e))?;
                result.push(
                    String::from_utf8(bytes)
                        .map_err(|error| self.not_utf8("a string", error.as_bytes()))?,
                );
            }
            Ok(result)
        })
    }

    /// The elements of a list, as cursors.
    pub fn get_list(&self) -> Result<Vec<Cursor<'s>>> {
        let slots = self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::List(list) = ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Err(Error::Usage(
                    format!("'{}' is not a list", self.attr_path_str()).into(),
                ));
            };
            Ok((0..list.len())
                .map(|index| list.get(index).expect("list length stays fixed"))
                .collect::<Vec<_>>())
        })?;
        Ok(slots
            .into_iter()
            .enumerate()
            .map(|(index, slot)| {
                let mut path = self.attr_path.clone();
                path.push(index.to_string());
                Cursor::new(self.session, slot, path)
            })
            .collect())
    }

    /// Whether the value is a derivation (`type = "derivation"`).
    pub fn is_derivation(&self) -> Result<bool> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(false);
            };
            let Some((kind, pos)) = attrs.get_with_pos(b"type") else {
                return Ok(false);
            };
            ctx.ev
                .force_at(&kind, pos)
                .map_err(|e| self.eval_err(ctx, e))?;
            if !matches!(
                ctx.ev.view(&kind).map_err(|e| self.eval_err(ctx, e))?,
                GcValueView::String(_)
            ) {
                return Ok(false);
            }
            Ok(ctx
                .ev
                .string_bytes(&kind)
                .map_err(|e| self.eval_err(ctx, e))?
                == b"derivation")
        })
    }

    /// `AttrCursor::forceDerivation`: the derivation's store path. The
    /// derivation is recorded in the session's virtual store as a side
    /// effect of evaluating `drvPath`.
    pub fn force_derivation(&self) -> Result<StorePath> {
        let drv_path = self.get_attr("drvPath")?.get_string()?;
        self.session
            .config()
            .store_dir()
            .parse(&drv_path)
            .map_err(Error::from)
    }

    /// Follows `path`, returning `None` if an attribute (or list index)
    /// along the way is missing. Errors from forcing propagate.
    pub fn find_along_attr_path(&self, path: &[String]) -> Result<Option<Cursor<'s>>> {
        let mut current = self.clone();
        for component in path {
            let next = current.with(|ctx, cell| {
                ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
                Ok(
                    match ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))? {
                        GcValueView::Attrs(attrs) => attrs.get(component.as_bytes()),
                        GcValueView::List(list) => component
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| list.get(index)),
                        _ => None,
                    },
                )
            })?;
            match next {
                Some(slot) => {
                    let mut path = current.attr_path.clone();
                    path.push(component.clone());
                    current = Cursor::new(self.session, slot, path);
                }
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// Applies the function at `fun` to this value (`nix eval --apply`).
    pub fn apply(&self, fun: &Cursor<'s>) -> Result<Cursor<'s>> {
        if !std::ptr::eq(self.session, fun.session) {
            return Err(Error::Usage(
                "a cursor belongs to a different session".into(),
            ));
        }
        let slot = self
            .session
            .with_slots(&fun.slot, &self.slot, |ctx, fun, arg| {
                ctx.ev.apply(&fun, &arg).map_err(|e| self.eval_err(ctx, e))
            })?;
        Ok(Cursor::new(self.session, slot, Vec::new()))
    }

    /// `EvalState::coerceToStorePath(attr->pos, *attr->value, …,
    /// error_ctx)` for the attribute `name` of this set: its value as a
    /// store path, Nix's errors placed at the attribute. `None` when the
    /// set lacks `name` (`nix bundle` reads the bundler result's `drvPath`
    /// and `outPath` this way).
    pub fn coerce_attr_to_store_path(
        &self,
        name: &str,
        error_ctx: &str,
    ) -> Result<Option<StorePath>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(None);
            };
            let Some((value, pos)) = attrs.get_with_pos(name.as_bytes()) else {
                return Ok(None);
            };
            ctx.ev
                .coerce_to_store_path(&value, pos, error_ctx)
                .map(Some)
                .map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// `EvalState::forceStringNoCtx(*attr->value, attr->pos, error_ctx)`
    /// for the attribute `name` of this set: a string that refers to no
    /// store path, Nix's errors placed at the attribute. `None` when the
    /// set lacks `name`.
    pub fn force_attr_string_no_ctx(&self, name: &str, error_ctx: &str) -> Result<Option<String>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(None);
            };
            let Some((value, pos)) = attrs.get_with_pos(name.as_bytes()) else {
                return Ok(None);
            };
            let bytes = ctx
                .ev
                .force_string_no_ctx(&value, pos, error_ctx)
                .map_err(|e| self.eval_err(ctx, e))?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|error| self.not_utf8("a string", error.as_bytes()))
        })
    }

    /// A deep, owned copy.
    pub fn to_owned(&self) -> Result<OwnedValue> {
        self.with(|ctx, cell| {
            ctx.ev
                .to_owned_value(&cell)
                .map_err(|e| self.eval_err(ctx, e))
        })
    }

    /// Rendered as `nix eval` would print it: bytes, as strings in the
    /// value are (Nix writes them as they are).
    pub fn render_bytes(&self, format: Format) -> Result<Vec<u8>> {
        self.with(|ctx, cell| self.session.render(ctx.ev, &cell, format))
    }

    /// [`Cursor::render_bytes`] as text, ill-formed UTF-8 replaced.
    pub fn render(&self, format: Format) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.render_bytes(format)?).into_owned())
    }

    /// Whether the forced value is an attribute set.
    pub fn is_attrs(&self) -> Result<bool> {
        Ok(self.value_type()? == ValueType::Attrs)
    }
}

#[cfg(test)]
mod tests {
    use super::{attr_path_to_string, parse_attr_path};

    #[test]
    fn managed_cursor_children_survive_collection_and_errors_do_not_reborrow_session() {
        let mut config = crate::Config {
            daemon: Some("unix:///nonexistent/brix-cursor-test".into()),
            teardown: crate::Teardown::Free,
            ..Default::default()
        };
        config.fetch.enabled = false;
        config.compiled.enabled = false;
        config.gc.stress = true;
        config.gc.chunk_bytes = 256;
        config.gc.threshold_bytes = 1024;
        let session = crate::Session::new(config).unwrap();
        let root = session
            .eval_expr_cursor(
                r#"{
            answer = 20 + 22;
            strings = [ "a" ("b" + "c") ];
            nested = [ { value = 7; } ];
            function = x: x + 1;
            failure = throw "cursor failure";
        }"#,
                "/",
            )
            .unwrap();
        let answer = root.get_attr("answer").unwrap();
        let strings = root.get_attr("strings").unwrap();
        let function = root.get_attr("function").unwrap();
        let failure = root.get_attr("failure").unwrap();
        let nested = root
            .find_along_attr_path(&["nested".into(), "0".into(), "value".into()])
            .unwrap()
            .unwrap();
        drop(root);
        session.with(|ctx| ctx.ev.collect()).unwrap();
        assert_eq!(answer.get_int().unwrap(), 42);
        assert_eq!(strings.get_list_of_strings().unwrap(), ["a", "bc"]);
        assert_eq!(nested.get_int().unwrap(), 7);
        let input = session.eval_expr_cursor("41", "/").unwrap();
        assert_eq!(input.apply(&function).unwrap().get_int().unwrap(), 42);
        assert!(failure
            .get_int()
            .unwrap_err()
            .to_string()
            .contains("cursor failure"));
        assert!(answer
            .get_bool()
            .unwrap_err()
            .to_string()
            .contains("not a Boolean"));
        session.with(|ctx| ctx.ev.collect()).unwrap();
        assert_eq!(answer.get_int().unwrap(), 42);
    }

    /// `printAttributeName` quotes names that are not identifiers and, as
    /// in a string literal, escapes `$` only where it starts `${`.
    #[test]
    fn attribute_paths_print_like_nix() {
        let path = |names: &[&str]| {
            attr_path_to_string(&names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            path(&["packages", "x86_64-linux", "a'b-c"]),
            "packages.x86_64-linux.a'b-c"
        );
        assert_eq!(path(&["a$b", "1x", "if"]), r#""a$b"."1x"."if""#);
        assert_eq!(path(&["a${b}", "q\"\\\n"]), r#""a\${b}"."q\"\\\n""#);
        assert_eq!(path(&[""]), r#""""#);
        assert_eq!(path(&[]), "");
    }

    /// `parseAttrPath`: `.` separates, `"…"` quotes a component.
    #[test]
    fn attr_paths_parse_like_nix() {
        assert_eq!(parse_attr_path("a.b.c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(
            parse_attr_path("a.\"b.c\".d").unwrap(),
            vec!["a", "b.c", "d"]
        );
        assert_eq!(parse_attr_path("").unwrap(), Vec::<String>::new());
        assert!(parse_attr_path("a.\"b").is_err());
    }
}

/// Read operations shared by session cursors and worker-local batch cursors.
/// Each operation returns owned handles or snapshots, never borrowed heap data.
pub trait ReadCursor: Clone {
    fn attr_path(&self) -> &[String];
    fn attr_path_str(&self) -> String;
    fn maybe_get_attr(&self, name: &str) -> Result<Option<Self>>;
    fn get_attr(&self, name: &str) -> Result<Self>;
    fn get_attrs(&self) -> Result<Vec<String>>;
    fn get_bool(&self) -> Result<bool>;
    fn get_string(&self) -> Result<String>;
    fn get_list_of_strings(&self) -> Result<Vec<String>>;
    fn find_along_attr_path(&self, path: &[String]) -> Result<Option<Self>>;
    fn force_derivation(&self) -> Result<StorePath>;
}

macro_rules! impl_read_cursor {
    ($ty:ty) => {
        impl crate::cursor::ReadCursor for $ty {
            fn attr_path(&self) -> &[String] {
                self.attr_path()
            }
            fn attr_path_str(&self) -> String {
                self.attr_path_str()
            }
            fn maybe_get_attr(&self, name: &str) -> Result<Option<Self>> {
                self.maybe_get_attr(name)
            }
            fn get_attr(&self, name: &str) -> Result<Self> {
                self.get_attr(name)
            }
            fn get_attrs(&self) -> Result<Vec<String>> {
                self.get_attrs()
            }
            fn get_bool(&self) -> Result<bool> {
                self.get_bool()
            }
            fn get_string(&self) -> Result<String> {
                self.get_string()
            }
            fn get_list_of_strings(&self) -> Result<Vec<String>> {
                self.get_list_of_strings()
            }
            fn find_along_attr_path(&self, path: &[String]) -> Result<Option<Self>> {
                self.find_along_attr_path(path)
            }
            fn force_derivation(&self) -> Result<StorePath> {
                self.force_derivation()
            }
        }
    };
}
#[cfg(brix_parallel)]
pub(crate) use impl_read_cursor;
impl_read_cursor!(Cursor<'_>);
