//! String and path coercions and the virtual store writes they cause.
//!
//! `coerceToString`, `coerceToPath`, `copyPathToStore` and `realisePath`
//! from `eval.cc` and `primops.cc`.

use brix_store::{CanonPath, ContentAddressMethod, SourcePath, StorePath};
use bstr::{BStr, BString};

use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{ContextElem, PathVal, SrcPos, Value, ValueRef};
use crate::Result;

/// The flags of `EvalState::coerceToString`: what becomes of a value
/// that is not a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coercion {
    /// `coerceMore`: also accept Booleans, integers, floats, null and lists
    /// (as `toString` does).
    pub more: bool,
    /// `copyToStore`: paths are copied to the store and become store
    /// paths; otherwise their string form is used.
    pub copy_to_store: bool,
    /// `canonicalizePath`: when neither copying nor canonicalising, a
    /// path's string is returned untouched.
    pub canonicalize: bool,
}

impl Coercion {
    /// Nix's defaults: paths are copied to the store (string
    /// interpolation, `substring`, `throw`, …).
    pub const COPY: Coercion = Coercion {
        more: false,
        copy_to_store: true,
        canonicalize: true,
    };
    /// A path is its canonical text, not copied (`coerceToPath`, URLs).
    pub const NO_COPY: Coercion = Coercion {
        more: false,
        copy_to_store: false,
        canonicalize: true,
    };
    /// A path is its text as it is (`baseNameOf`, `dirOf`).
    pub const RAW: Coercion = Coercion {
        more: false,
        copy_to_store: false,
        canonicalize: false,
    };
    /// `toString`: more types, paths not copied.
    pub const MORE: Coercion = Coercion {
        more: true,
        copy_to_store: false,
        canonicalize: true,
    };
    /// Derivation attributes: more types, paths copied.
    pub const MORE_COPY: Coercion = Coercion {
        more: true,
        copy_to_store: true,
        canonicalize: true,
    };
}

/// `std::to_string(double)`: `%f`.
pub fn float_to_string(f: f64) -> String {
    crate::print::format_float_f(f)
}

impl<'a> Evaluator<'a> {
    /// `CanonPath(s)` for a path made from Nix strings, whose bytes it
    /// keeps as they are (`/a + "\xff"` is the path `/a\xff`): Nix throws
    /// `BadCanonPath` (no position) for a NUL byte, which no Nix string
    /// can hold but a host-supplied one could.
    pub(crate) fn canon_path(&self, s: &[u8]) -> Result<CanonPath> {
        CanonPath::new(s).map_err(|e| self.error(ErrorKind::Eval, e.message_bytes()))
    }

    /// `EvalState::coerceToString`, with its flags in `coercion`.
    pub fn coerce_to_string(
        &self,
        pos: SrcPos,
        cell: ValueRef<'a>,
        context: &mut StringContext<'a>,
        error_ctx: &str,
        coercion: Coercion,
    ) -> Result<&'a [u8]> {
        let _guard = self.enter(pos)?;
        let v = self.force(cell, pos)?;
        self.coerce_forced_to_string(pos, Some(cell), v, context, error_ctx, coercion)
    }

    /// Forces `cell` as [`Evaluator::coerce_to_string`] does, one call
    /// level deeper (which Nix's `max-call-depth` counts): for builtins
    /// that look at the value before deciding how to coerce it.
    pub(crate) fn force_for_coercion(&self, cell: ValueRef<'a>, pos: SrcPos) -> Result<Value<'a>> {
        let _guard = self.enter(pos)?;
        self.force(cell, pos)
    }

    /// [`Evaluator::coerce_value_to_string`] for the operations that keep
    /// lazy hashes (see [`crate::lazy_tree`]): appends `v`'s text to `out`,
    /// where a string's lazy hashes, and the text of a path into a lazy
    /// tree, stay lazy. Anything else is coerced as usual.
    pub(crate) fn coerce_into_lazy(
        &self,
        pos: SrcPos,
        v: Value<'a>,
        out: &mut crate::lazy_tree::LazyStrBuilder<'a>,
        error_ctx: &str,
        coercion: Coercion,
    ) -> Result<()> {
        match v {
            Value::String(s) if s.is_lazy() => {
                out.push_str(s);
                Ok(())
            }
            // Without copying, a path is its text; only a canonicalising
            // coercion gives a store path its `Path` context.
            Value::Path(p) if !coercion.copy_to_store && p.accessor == self.root_accessor => {
                self.push_path_text(out, p.path, coercion.canonicalize)
            }
            _ => {
                let bytes =
                    self.coerce_value_to_string(pos, v, &mut out.context, error_ctx, coercion)?;
                out.push_bytes(bytes);
                Ok(())
            }
        }
    }

    /// `coerceToString` on a value that is already forced. The `+`
    /// operator and interpolation have the value in hand; the cell that
    /// [`Evaluator::coerce_to_string`] takes is only needed to call a
    /// `__toString` function, so it is allocated just in that case.
    pub fn coerce_value_to_string(
        &self,
        pos: SrcPos,
        v: Value<'a>,
        context: &mut StringContext<'a>,
        error_ctx: &str,
        coercion: Coercion,
    ) -> Result<&'a [u8]> {
        let _guard = self.enter(pos)?;
        self.coerce_forced_to_string(pos, None, v, context, error_ctx, coercion)
    }

    /// The result borrows the arena: a string's own bytes, or bytes
    /// allocated for the coerced form of another value. Nothing is copied
    /// for the common case of a string.
    fn coerce_forced_to_string(
        &self,
        pos: SrcPos,
        cell: Option<ValueRef<'a>>,
        v: Value<'a>,
        context: &mut StringContext<'a>,
        error_ctx: &str,
        coercion: Coercion,
    ) -> Result<&'a [u8]> {
        match v {
            Value::String(s) => {
                let (bytes, string_context) = self.str_parts(s)?;
                context.extend_from_slice(string_context);
                Ok(bytes)
            }
            Value::Path(p) => {
                if !coercion.canonicalize && !coercion.copy_to_store {
                    return Ok(match self.path_text(p)? {
                        std::borrow::Cow::Borrowed(s) => s,
                        std::borrow::Cow::Owned(s) => self.alloc_str(&s),
                    });
                }
                self.coerce_path_to_string(pos, p, context, coercion.copy_to_store)
            }
            Value::Attrs(b) => {
                let cell = cell.unwrap_or_else(|| self.alloc_cell(v));
                if let Some(s) = self.try_attrs_to_string(pos, cell, v, context, coercion)? {
                    return Ok(s);
                }
                match b.get(sym::OUT_PATH) {
                    Some(out) => {
                        self.coerce_to_string(pos, out.value, context, error_ctx, coercion)
                    }
                    None => Err(self.cannot_coerce(v, pos, error_ctx)),
                }
            }
            Value::Bool(true) if coercion.more => Ok(b"1"),
            Value::Bool(false) | Value::Null if coercion.more => Ok(b""),
            Value::Int(i) if coercion.more => Ok(self.alloc_str(i.to_string().as_bytes())),
            Value::Float(f) if coercion.more => Ok(self.alloc_str(float_to_string(f).as_bytes())),
            Value::List(l) if coercion.more => {
                let mut out = Vec::new();
                let n = l.elems.len();
                for (i, &elem) in l.elems.iter().enumerate() {
                    // Nested lists recurse as deep as `max-call-depth` lets
                    // them: on a fresh stack segment when needed.
                    let part = self
                        .with_stack(|| {
                            self.coerce_to_string(
                                pos,
                                elem,
                                context,
                                "while evaluating one element of the list",
                                coercion,
                            )
                        })
                        .inspect_err(|e| {
                            self.add_trace(e, pos, error_ctx);
                        })?;
                    out.extend_from_slice(part);
                    // Nix's "not quite correct" rule: no space after an
                    // element that is an empty list.
                    let elem_is_empty_list =
                        matches!(elem.get(), Value::List(l2) if l2.elems.is_empty());
                    if i + 1 < n && !elem_is_empty_list {
                        out.push(b' ');
                    }
                }
                Ok(self.alloc_str(&out))
            }
            _ => Err(self.cannot_coerce(v, pos, error_ctx)),
        }
    }

    fn cannot_coerce(&self, v: Value<'a>, pos: SrcPos, error_ctx: &str) -> crate::Error {
        self.error(
            ErrorKind::TypeError,
            msg!(
                "cannot coerce ",
                v.show_type(),
                " to a string: ",
                self.print_for_error(v)
            ),
        )
        .with_trace(self.position(pos), error_ctx)
    }

    /// `tryAttrsToString`: `__toString` support.
    fn try_attrs_to_string(
        &self,
        pos: SrcPos,
        cell: ValueRef<'a>,
        v: Value<'a>,
        context: &mut StringContext<'a>,
        coercion: Coercion,
    ) -> Result<Option<&'a [u8]>> {
        let Value::Attrs(b) = v else { return Ok(None) };
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
            Coercion {
                canonicalize: true,
                ..coercion
            },
        )?))
    }

    /// `coercePathToString`.
    fn coerce_path_to_string(
        &self,
        pos: SrcPos,
        p: &'a PathVal<'a>,
        context: &mut StringContext<'a>,
        copy_to_store: bool,
    ) -> Result<&'a [u8]> {
        if copy_to_store {
            let sp =
                self.copy_path_to_store(context, p, self.determine_pos(Value::Path(p), pos))?;
            Ok(self.alloc_str(self.config.store_dir.display(&sp).as_bytes()))
        } else {
            let text = self.path_text(p)?;
            if p.accessor == self.root_accessor {
                if let Some(sp) = self.maybe_store_path(&*text) {
                    context.insert(ContextElem::Path(self.arena_str(sp.as_str())));
                }
            }
            Ok(match text {
                std::borrow::Cow::Borrowed(s) => s,
                std::borrow::Cow::Owned(s) => self.alloc_str(&s),
            })
        }
    }

    /// `EvalState::copyPathToStore`: "adds" the path to the (virtual)
    /// store and returns its store path.
    pub fn copy_path_to_store(
        &self,
        context: &mut StringContext<'a>,
        p: &PathVal<'_>,
        pos: SrcPos,
    ) -> Result<StorePath> {
        // A path copied before gives the same result again: every step
        // below is a function of the path (trees are immutable for the
        // evaluation, as `srcToStore` assumes, and the allow list only
        // grows), and allowing the destination again changes nothing.
        // Derivations mention the same `./builder.sh` or patch over and
        // over, so a repeat skips straight to the result.
        if let Some((dst, base)) = self
            .path_copies
            .borrow()
            .get(&crate::evaluator::PathCopyLookup(p.accessor, p.path))
        {
            crate::stats::Stats::bump(&self.stats.store_cache_hits);
            context.insert(ContextElem::Opaque(base));
            return Ok(dst.clone());
        }
        if p.path.ends_with(b".drv") {
            return Err(self.error(
                ErrorKind::Eval,
                "file names are not allowed to end in '.drv'",
            ));
        }
        // Access control comes before any look at the file system.
        self.check_path_access(p)?;
        let name = self.compute_base_name(p)?;
        let source = self.resolve_symlinks_ancestors(p)?;
        // Nix's `srcToStore`: trees are immutable for the evaluation, so
        // the same source path always yields the same store path.
        let cached = self.src_to_store.borrow().get(&source).cloned();
        let dst = match cached {
            Some(dst) => {
                crate::stats::Stats::bump(&self.stats.store_cache_hits);
                dst
            }
            None => {
                let dst = self.add_path_to_store(
                    &source,
                    &name,
                    ContentAddressMethod::NixArchive,
                    None,
                    pos,
                )?;
                self.src_to_store.borrow_mut().insert(source, dst.clone());
                dst
            }
        };
        self.allow_path(self.store_path_mount_point(&dst));
        let base = self.arena_str(dst.as_str());
        context.insert(ContextElem::Opaque(base));
        self.path_copies
            .borrow_mut()
            .insert((p.accessor, p.path.into()), (dst.clone(), base));
        Ok(dst)
    }

    /// `EvalState::computeBaseName`: the name a path is copied to the
    /// store under. A store path itself (not a path inside one) is named
    /// after the path that fetching it into the store again under its own
    /// name gives (`fetchToStore(…, DryRun, storePath->name())`), so the
    /// copy is called `<hash>-<name>`; any other path after its last
    /// component, the root `source` (`SourcePath::baseName`). The name is
    /// bytes: a file name that is not UTF-8 is refused as a store path
    /// name later, with the path computed from those bytes, as in Nix.
    pub fn compute_base_name(&self, p: &PathVal<'_>) -> Result<BString> {
        if p.accessor == self.root_accessor {
            // A lazy tree's root stands for its store path.
            if let Some(id) = self.lazy_root(p.path) {
                return Ok(self.materialize(id)?.as_str().into());
            }
            if let Some(sp) = self.maybe_store_path(p.path) {
                if p.path == self.config.store_dir.display(&sp).as_bytes() {
                    return Ok(self.refetched_store_path(&sp)?.as_str().into());
                }
            }
        }
        Ok(self
            .source_path(p)
            .path
            .base_name()
            .unwrap_or(BStr::new("source"))
            .to_owned())
    }

    /// The store path `sp`'s contents get when fetched into the store
    /// again as a NAR under `sp`'s own name, without references.
    fn refetched_store_path(&self, sp: &StorePath) -> Result<StorePath> {
        // A source tree added that way (a fetched input, a copied path)
        // is its own content-addressed path.
        if matches!(
            self.store.borrow().get(sp),
            Some(brix_store::StoreObject::SourceTree {
                method: ContentAddressMethod::NixArchive,
                filtered: false,
                ..
            })
        ) {
            return Ok(sp.clone());
        }
        // Anything else (a `toFile` text, a flat or filtered copy, a build
        // output) is hashed as Nix hashes it: read through the root file
        // system, where the evaluator mounted what it added and the host
        // serves what was built. An output nobody built cannot be read,
        // and that error is the answer, as it is in Nix.
        let path = self.store_path_mount_point(sp);
        let accessor = self.accessor(self.root_accessor);
        let (hash, _) = brix_store::nar::hash(&*accessor, &path, brix_store::HashAlgo::Sha256)
            .map_err(|e| self.error(ErrorKind::FileSystem, e.message_bytes()))?;
        let ca = brix_store::ContentAddressWithReferences::from_parts(
            ContentAddressMethod::NixArchive,
            hash,
            brix_store::StoreReferences::default(),
        )
        .expect("a NAR hash without references is a valid content address");
        self.config
            .store_dir
            .make_fixed_output_path_from_ca(sp.name(), &ca)
            .map_err(|e| self.error(ErrorKind::Eval, e.to_string()))
    }

    /// Resolves symlinks in the ancestors of a path (what copying does
    /// before hashing). Symlinks inside the tree are preserved.
    pub fn resolve_symlinks_ancestors(&self, p: &PathVal<'_>) -> Result<SourcePath> {
        self.resolve_symlinks_of(p, false)
    }

    /// `SourcePath::resolveSymlinks()` in full: a symlink at the path
    /// itself is followed too, so its target is what gets copied (what
    /// `addPath`, behind `builtins.path` and `filterSource`, does, where
    /// `copyPathToStore` resolves only the ancestors).
    pub(crate) fn resolve_symlinks_fully(&self, p: &PathVal<'_>) -> Result<SourcePath> {
        self.resolve_symlinks_of(p, true)
    }

    fn resolve_symlinks_of(&self, p: &PathVal<'_>, resolve_final: bool) -> Result<SourcePath> {
        let sp = self.source_path(p);
        let accessor = self.accessor(p.accessor);
        match accessor.resolve_symlinks(&sp.path, resolve_final) {
            Ok(path) => Ok(SourcePath::new(p.accessor, path)),
            Err(e) => Err(self.error(ErrorKind::FileSystem, e.message_bytes())),
        }
    }

    /// `coerceToPath`: a path value, a `__toString` result that is one, or
    /// a string that is an absolute path.
    pub fn coerce_to_path(
        &self,
        pos: SrcPos,
        cell: ValueRef<'a>,
        context: &mut StringContext<'a>,
        error_ctx: &str,
    ) -> Result<PathVal<'a>> {
        let v = self
            .force(cell, pos)
            .map_err(|e| self.traced(e, pos, error_ctx))?;
        if let Value::Path(p) = v {
            return Ok(*p);
        }
        if let Value::Attrs(b) = v {
            if let Some(ts) = b.get(sym::TO_STRING) {
                let f = self.force(ts.value, pos)?;
                let r = self.call(f, &[cell], pos)?;
                let rc = self.alloc_cell(r);
                return self.coerce_to_path(pos, rc, context, error_ctx);
            }
        }
        // A string naming a file in a lazy tree is turned into a path at
        // the tree's placeholder, its hash still unknown.
        if let Value::String(s) = v {
            if let Some(path) = self.lazy_str_path(s) {
                context.extend_from_slice(crate::lazy_tree::context_without_hashes(s));
                let canon = self.canon_path(&path)?;
                return Ok(PathVal {
                    accessor: self.root_accessor,
                    path: self.bump.alloc_slice_copy(canon.as_bytes()),
                });
            }
        }
        let s = self.coerce_to_string(pos, cell, context, error_ctx, Coercion::NO_COPY)?;
        if !s.starts_with(b"/") {
            return Err(self
                .error(
                    ErrorKind::Eval,
                    msg!("string '", s, "' doesn't represent an absolute path"),
                )
                .with_trace(self.position(pos), error_ctx));
        }
        // The string's bytes are the path's, whatever they are.
        let canon = self.canon_path(s)?;
        Ok(PathVal {
            accessor: self.root_accessor,
            path: self.bump.alloc_slice_copy(canon.as_bytes()),
        })
    }

    /// `coerceToStorePath`.
    pub fn coerce_to_store_path(
        &self,
        pos: SrcPos,
        cell: ValueRef<'a>,
        context: &mut StringContext<'a>,
        error_ctx: &str,
    ) -> Result<StorePath> {
        let s = self.coerce_to_string(pos, cell, context, error_ctx, Coercion::NO_COPY)?;
        match self.config.store_dir.maybe_parse(s) {
            Some(p) => Ok(p),
            None => Err(self
                .error(
                    ErrorKind::Eval,
                    msg!(
                        "cannot coerce '",
                        s,
                        "' to a store path because it is not a subpath of the Nix store"
                    ),
                )
                .with_trace(self.position(pos), error_ctx)),
        }
    }

    /// `EvalState::realisePath`: coerce to a path and make sure whatever
    /// its context refers to has been built.
    pub fn realise_path(&self, pos: SrcPos, cell: ValueRef<'a>) -> Result<PathVal<'a>> {
        let mut context = StringContext::new();
        let p = self.coerce_to_path(
            SrcPos::NONE,
            cell,
            &mut context,
            "while realising the context of a path",
        )?;
        if !context.is_empty() && p.accessor == self.root_accessor {
            self.realise_context(&context, true).inspect_err(|e| {
                self.add_trace(
                    e,
                    pos,
                    msg!(
                        "while realising the context of path '",
                        self.show_path(&p),
                        "'"
                    ),
                );
            })?;
        }
        Ok(p)
    }

    /// `EvalState::realiseContext`: build what the context needs built
    /// (import-from-derivation) and check the rest exists.
    pub fn realise_context(&self, context: &StringContext<'_>, is_ifd: bool) -> Result<()> {
        let mut builds: Vec<crate::host::BuildRequest> = Vec::new();
        for c in context {
            match *c {
                ContextElem::Built { drv, output } => {
                    let drv_path = StorePath::new(drv)
                        .map_err(|e| self.error(ErrorKind::Eval, e.to_string()))?;
                    match builds.iter_mut().find(|b| b.drv_path == drv_path) {
                        Some(b) => b.outputs.push(output.into()),
                        None => builds.push(crate::host::BuildRequest {
                            drv_path,
                            outputs: vec![output.into()],
                        }),
                    }
                }
                ContextElem::Opaque(p) | ContextElem::DrvDeep(p) => {
                    let sp = StorePath::new(p)
                        .map_err(|e| self.error(ErrorKind::Eval, e.to_string()))?;
                    let valid = self.store.borrow().contains(&sp)
                        || self
                            .host
                            .is_valid_path(&sp)
                            .map_err(|e| self.error(ErrorKind::Eval, e))?;
                    if !valid {
                        let error = self.error(
                            ErrorKind::Eval,
                            format!("path '{}' is not valid", self.config.store_dir.display(&sp)),
                        );
                        error.invalid_path.set(sp).expect("a new error has no path");
                        return Err(error);
                    }
                }
                ContextElem::Path(_) | ContextElem::LazyPath(_) => {}
                ContextElem::LazyHash { .. } => {
                    unreachable!("an accumulated context never holds lazy hashes")
                }
            }
        }
        if builds.is_empty() {
            return Ok(());
        }
        if is_ifd && !self.config.allow_import_from_derivation {
            return Err(self.error(
                ErrorKind::Unsupported,
                msg!(
                    "cannot build '",
                    self.config.store_dir.display(&builds[0].drv_path).to_string(),
                    "^",
                    bstr::join(",", &builds[0].outputs),
                    "' during evaluation because the option 'allow-import-from-derivation' is disabled"
                ),
            ));
        }
        let closure = self
            .host
            .build(&builds, self)
            .map_err(|msg| self.error(ErrorKind::Unsupported, msg))?;
        if is_ifd {
            // `allowClosure`: the outputs and what they refer to, nothing
            // else of the store.
            for path in &closure {
                self.allow_path(self.store_path_mount_point(path));
            }
        }
        Ok(())
    }
}

impl<'a> Evaluator<'a> {
    /// Where the contents of the source object `path` are read from, and
    /// how it was ingested.
    fn source_object(
        &self,
        path: &StorePath,
    ) -> std::result::Result<
        (
            std::sync::Arc<dyn brix_store::SourceAccessor>,
            CanonPath,
            brix_store::ContentAddress,
        ),
        String,
    > {
        use brix_store::StoreObject;
        let object = self
            .store
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| format!("path '{}' is not in the virtual store", path))?;
        match object {
            StoreObject::SourceTree {
                source,
                method,
                hash,
                filtered,
            } => {
                // Filtered copies are mounted at their store path with the
                // entries accepted during hashing. Read that view to reproduce
                // the same contents without re-running the Nix filter or
                // accidentally uploading the unfiltered source.
                let (accessor, source_path) = if filtered {
                    (
                        self.accessor(self.root_accessor),
                        self.store_path_mount_point(path),
                    )
                } else {
                    (self.accessor(source.accessor), source.path)
                };
                Ok((
                    accessor,
                    source_path,
                    brix_store::ContentAddress { method, hash },
                ))
            }
            StoreObject::Text { .. } | StoreObject::Derivation(_) => Err(format!(
                "'{}' is not a source object; its bytes are in the store object itself",
                path
            )),
        }
    }
}

impl<'a> crate::host::EvalStoreView for Evaluator<'a> {
    fn object(&self, path: &StorePath) -> Option<brix_store::StoreObject> {
        self.store.borrow().get(path).cloned()
    }

    fn source_nar(&self, path: &StorePath) -> std::result::Result<crate::host::SourceNar, String> {
        use brix_store::{ContentAddressMethod, HashAlgo};
        let (accessor, source_path, ca) = self.source_object(path)?;
        let (hash, size) = match ca.method {
            // The object's hash is its NAR's: only the size is missing,
            // which takes no reading where the accessor knows file sizes.
            ContentAddressMethod::NixArchive if ca.hash.algo() == HashAlgo::Sha256 => (
                ca.hash,
                brix_store::nar::size(&*accessor, &source_path).map_err(|e| e.to_string())?,
            ),
            ContentAddressMethod::NixArchive => {
                brix_store::nar::hash(&*accessor, &source_path, HashAlgo::Sha256)
                    .map_err(|e| e.to_string())?
            }
            ContentAddressMethod::Flat => {
                let mut hasher = brix_store::Hasher::new(HashAlgo::Sha256);
                brix_store::nar::dump_flat(&*accessor, &source_path, &mut hasher)
                    .map_err(|e| e.to_string())?;
                hasher.finish()
            }
            other => {
                return Err(format!(
                    "source '{}' uses the '{}' ingestion method, which cannot be uploaded",
                    path,
                    other.render()
                ))
            }
        };
        Ok(crate::host::SourceNar { hash, size, ca })
    }

    fn write_source_nar(
        &self,
        path: &StorePath,
        out: &mut dyn std::io::Write,
    ) -> std::result::Result<(), String> {
        use brix_store::ContentAddressMethod;
        let (accessor, source_path, ca) = self.source_object(path)?;
        match ca.method {
            ContentAddressMethod::NixArchive => {
                brix_store::nar::dump(&*accessor, &source_path, out)
            }
            ContentAddressMethod::Flat => brix_store::nar::dump_flat(&*accessor, &source_path, out),
            other => {
                return Err(format!(
                    "source '{}' uses the '{}' ingestion method, which cannot be uploaded",
                    path,
                    other.render()
                ))
            }
        }
        .map_err(|e| e.to_string())
    }
}
