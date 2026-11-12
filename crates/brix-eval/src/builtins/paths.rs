//! Path and file built-ins: `pathExists`, `readFile`, `readDir`,
//! `readFileType`, `findFile`, `hashFile`, `toFile`, `filterSource`,
//! `path`, `storePath`, `baseNameOf`, `dirOf`, `toPath`, `placeholder`.

use std::collections::BTreeSet;

use brix_store::{CanonPath, ContentAddressMethod, FileType, Hash, HashAlgo, StorePath};
use bstr::BString;

use crate::builtins::{reg, require_attr};
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{Attr, ContextElem, PathVal, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "placeholder", 1, prim_placeholder);
    reg(ev, "__toPath", 1, prim_to_path);
    reg(ev, "__storePath", 1, prim_store_path);
    reg(ev, "__pathExists", 1, prim_path_exists);
    reg(ev, "baseNameOf", 1, prim_base_name_of);
    reg(ev, "dirOf", 1, prim_dir_of);
    reg(ev, "__readFile", 1, prim_read_file);
    reg(ev, "__findFile", 2, prim_find_file);
    reg(ev, "__hashFile", 2, prim_hash_file);
    reg(ev, "__readFileType", 1, prim_read_file_type);
    reg(ev, "__readDir", 1, prim_read_dir);
    reg(ev, "__toFile", 2, prim_to_file);
    reg(ev, "__filterSource", 2, prim_filter_source);
    reg(ev, "__path", 1, prim_path);
}

fn prim_placeholder<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.placeholder",
    )?;
    // The output name's bytes are hashed as they are.
    Ok(ev.mk_string(&brix_store::derivation::hash_placeholder(name)))
}

fn prim_to_path<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let p = ev.coerce_to_path(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the first argument passed to builtins.toPath",
    )?;
    Ok(ev.mk_string_ctx(&ev.path_text(&p)?, &ctx))
}

fn prim_store_path<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    _args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    Err(ev.error_at(
        ErrorKind::Eval,
        "'builtins.storePath' is not allowed in pure evaluation mode",
        pos,
    ))
}

fn prim_path_exists<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    // A lazy hash never holds `/` or `.`, so its placeholder answers this as
    // the hash would.
    let must_be_dir = matches!(v, Value::String(s)
        if s.raw_bytes().ends_with(b"/") || s.raw_bytes().ends_with(b"/."));
    let r: Result<bool> = (|| {
        let p = ev.realise_path(pos, args[0])?;
        let st = ev.lstat(&p, must_be_dir)?;
        Ok(st.is_some_and(|st| !must_be_dir || st.file_type == FileType::Directory))
    })();
    match r {
        Ok(b) => Ok(Value::Bool(b)),
        Err(e) if e.kind == ErrorKind::RestrictedPath => Ok(Value::Bool(false)),
        Err(e) => Err(e),
    }
}

/// `legacyBaseNameOf`: everything after the last slash, ignoring one
/// trailing slash.
#[cfg(test)]
fn legacy_base_name_of(path: &[u8]) -> &[u8] {
    let (start, end) = legacy_base_name_range(path);
    &path[start..end]
}

/// Where [`legacy_base_name_of`]'s result lies in `path`.
fn legacy_base_name_range(path: &[u8]) -> (usize, usize) {
    if path.is_empty() {
        return (0, 0);
    }
    let mut last = path.len() - 1;
    if path[last] == b'/' && last > 0 {
        last -= 1;
    }
    let start = path[..=last]
        .iter()
        .rposition(|&c| c == b'/')
        .map_or(0, |p| p + 1);
    (start, (last + 1).max(start))
}

fn prim_base_name_of<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    const CTX: &str = "while evaluating the first argument passed to builtins.baseNameOf";
    let v = ev.force_for_coercion(args[0], pos)?;
    // The base name is cut at a `/`, which no hash holds, so a lazy hash
    // is either wholly in it or not at all.
    if ev.may_be_lazy_text(v) {
        let s = ev.lazy_text_value(pos, v, CTX, false)?;
        let (start, end) = legacy_base_name_range(s.raw_bytes());
        return ev.str_slice(s, start, end);
    }
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(pos, args[0], &mut ctx, CTX, Coercion::RAW)?;
    let (start, end) = legacy_base_name_range(s);
    Ok(ev.mk_string_arena(&s[start..end], &ctx))
}

fn prim_dir_of<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    if let Value::Path(p) = v {
        // A path value is canonical: its parent is the bytes before the
        // last `/`, and the root is its own parent.
        let parent: &[u8] = match p.path.iter().rposition(|&c| c == b'/') {
            Some(i) if i > 0 => &p.path[..i],
            _ => b"/",
        };
        return Ok(Value::Path(ev.bump.alloc(PathVal {
            accessor: p.accessor,
            path: parent,
        })));
    }
    const CTX: &str = "while evaluating the first argument passed to 'builtins.dirOf'";
    // Cut at a `/`, which no hash holds: a lazy hash stays whole or goes.
    if ev.may_be_lazy_text(v) {
        let s = ev.lazy_text_value(pos, v, CTX, false)?;
        let context = crate::lazy_tree::context_without_hashes(s);
        return match s.raw_bytes().iter().rposition(|&c| c == b'/') {
            None => Ok(ev.mk_string_view(b".", context)),
            Some(0) => Ok(ev.mk_string_view(b"/", context)),
            Some(i) => ev.str_slice(s, 0, i),
        };
    }
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(pos, args[0], &mut ctx, CTX, Coercion::RAW)?;
    let out: &[u8] = match s.iter().rposition(|&c| c == b'/') {
        None => b".",
        Some(0) => b"/",
        Some(i) => &s[..i],
    };
    Ok(ev.mk_string_arena(out, &ctx))
}

fn prim_read_file<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let p = ev.realise_path(pos, args[0])?;
    let contents = ev.read_file(&p)?;
    if contents.contains(&0) {
        return Err(ev.error_at(
            ErrorKind::Eval,
            msg!(
                "the contents of the file '",
                ev.show_path(&p),
                "' cannot be represented as a Nix string"
            ),
            pos,
        ));
    }
    // A store path's references that occur in the text become context.
    let mut ctx: Vec<ContextElem<'a>> = Vec::new();
    if p.accessor == ev.root_accessor {
        if let Some(sp) = ev.maybe_store_path(p.path) {
            let refs: Vec<StorePath> = ev
                .store
                .borrow()
                .get(&sp)
                .map(|obj| match obj {
                    brix_store::StoreObject::Text { references, .. } => {
                        references.iter().cloned().collect()
                    }
                    brix_store::StoreObject::Derivation(d) => d.references().into_iter().collect(),
                    brix_store::StoreObject::SourceTree { .. } => Vec::new(),
                })
                .unwrap_or_default();
            for r in refs {
                if contents
                    .windows(r.hash_part().len())
                    .any(|w| w == r.hash_part().as_bytes())
                {
                    ctx.push(ContextElem::Opaque(ev.arena_str(r.as_str())));
                }
            }
        }
    }
    ctx.sort();
    ctx.dedup();
    Ok(ev.alloc_string_value(&contents, &ctx))
}

/// `builtins.findFile` (`EvalState::findFile`): look a name up in a search
/// path. Prefixes, paths and the name are bytes, as in Nix. The `<nix/…>`
/// prefix resolves to the evaluator's built-in expressions.
fn prim_find_file<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let elems = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.findFile",
    )?;
    let mut lookup: Vec<(&'a [u8], &'a [u8])> = Vec::with_capacity(elems.len());
    for &e in elems {
        let b = ev.force_attrs(
            e,
            pos,
            "while evaluating an element of the list passed to builtins.findFile",
        )?;
        let prefix: &'a [u8] = match b.get(sym::PREFIX) {
            Some(a) => ev.force_string_no_ctx(
                a.value,
                pos,
                "while evaluating the `prefix` attribute of an element of the list passed to builtins.findFile",
            )?,
            None => b"",
        };
        let path_attr = require_attr(ev, b, sym::PATH, "in an element of the __nixPath")?;
        let mut ctx = StringContext::new();
        let path = ev.coerce_to_string(
            pos,
            path_attr.value,
            &mut ctx,
            "while evaluating the `path` attribute of an element of the list passed to builtins.findFile", Coercion::NO_COPY)?;
        ev.realise_context(&ctx, false)
            .map_err(|e| match e.invalid_path.get() {
                Some(invalid) => ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "cannot find '",
                        path,
                        "', since path '",
                        invalid.as_str(),
                        "' is not valid"
                    ),
                    pos,
                ),
                None => e,
            })?;
        lookup.push((prefix, path));
    }
    let name = ev.force_string_no_ctx(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.findFile",
    )?;
    for &(prefix, path) in &lookup {
        let Some(suffix) = suffix_if_potential_match(prefix, name) else {
            continue;
        };
        // This host resolves absolute entries only. Nix also resolves
        // relative entries against process cwd and handles pseudo-URLs;
        // see KNOWN_DEVIATIONS.md.
        if !path.starts_with(b"/") {
            continue;
        }
        // `rootPath(value)`.
        let base = ev.canon_path(path)?;
        // `resolveLookupPathPath`: the entry itself must exist (access
        // errors propagate); a missing one is skipped with a warning.
        let entry = PathVal {
            accessor: ev.root_accessor,
            path: ev.bump.alloc_slice_copy(base.as_bytes()),
        };
        if ev.lstat(&entry, true)?.is_none() {
            // `logWarning`, laid out like an error.
            ev.host.log(&crate::terminal::indent_message(
                "warning",
                msg!(
                    "Nix search path entry '",
                    path,
                    "' does not exist, ignoring"
                ),
            ));
            continue;
        }
        // `r.path / CanonPath(suffix)`: the suffix is canonicalised on its
        // own, so `..` in it never leaves the entry.
        let full = base.join(&ev.canon_path(suffix)?);
        let p = PathVal {
            accessor: ev.root_accessor,
            path: ev.bump.alloc_slice_copy(full.as_bytes()),
        };
        // `pathExists` here propagates access errors, as in Nix.
        if ev.lstat(&p, true)?.is_some() {
            return Ok(Value::Path(ev.bump.alloc(p)));
        }
    }
    if name.starts_with(b"nix/") {
        // `CanonPath(path.substr(3))`: `/…`, canonicalised.
        let corepkgs = ev.corepkgs_accessor();
        let path = ev.canon_path(&name[3..])?;
        return Ok(ev.mk_path(corepkgs, path.as_bytes()));
    }
    // Pure evaluation: `EvalState::findFile` reports the lookup itself as
    // impossible rather than the file as missing.
    Err(ev.error_at(
        ErrorKind::Thrown,
        msg!(
            "cannot look up '<",
            name,
            ">' in pure evaluation mode (use '--impure' to override)"
        ),
        pos,
    ))
}

/// `LookupPath::Prefix::suffixIfPotentialMatch`: what follows `prefix` in
/// `path`, after the `/` that must separate a non-empty prefix from the
/// rest, if `prefix` starts `path` that way.
fn suffix_if_potential_match<'s>(prefix: &[u8], path: &'s [u8]) -> Option<&'s [u8]> {
    let n = prefix.len();
    let need_separator = n > 0 && n < path.len();
    if need_separator && path[n] != b'/' {
        return None;
    }
    if !path.starts_with(prefix) {
        return None;
    }
    Some(&path[if need_separator { n + 1 } else { n }..])
}

fn prim_hash_file<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let algo_s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashFile",
    )?;
    let algo = crate::builtins::strings::parse_hash_algo(ev, algo_s, pos)?;
    let p = ev.realise_path(pos, args[1])?;
    let contents = ev.read_file(&p)?;
    Ok(ev.mk_string(&Hash::of(algo, &contents).to_string(brix_store::HashFormat::Base16, false)))
}

fn file_type_string(t: FileType) -> &'static str {
    match t {
        FileType::Regular => "regular",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        _ => "unknown",
    }
}

fn prim_read_file_type<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let p = ev.realise_path(pos, args[0])?;
    let st = ev.lstat_required(&p)?;
    Ok(ev.mk_string(file_type_string(st.file_type)))
}

fn prim_read_dir<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let p = ev.realise_path(pos, args[0])?;
    let entries = ev.read_dir(&p)?;
    // Where a child's path starts: the directory's, without a trailing
    // `/` (the root's is empty).
    let mut dir_len = p.path.len();
    while dir_len > 0 && p.path[dir_len - 1] == b'/' {
        dir_len -= 1;
    }
    let dir = &p.path[..dir_len];
    let mut attrs: Vec<Attr<'a>> = Vec::with_capacity(entries.len());
    for (name, ty) in entries {
        let value = match ty {
            Some(t) => ev.alloc_cell(ev.mk_string(file_type_string(t))),
            None => {
                // `<dir>/<name>`, the name's bytes as they are.
                let child = ev
                    .bump
                    .alloc_slice_fill_copy(dir.len() + 1 + name.len(), b'/');
                child[..dir.len()].copy_from_slice(dir);
                child[dir.len() + 1..].copy_from_slice(&name);
                let child = ev.alloc_cell(Value::Path(ev.bump.alloc(PathVal {
                    accessor: p.accessor,
                    path: child,
                })));
                ev.mk_app(ev.builtin("readFileType"), child)
            }
        };
        // The name is the attribute's, bytes and all.
        attrs.push(ev.attr_cell(ev.intern(&name), value));
    }
    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
}

fn prim_to_file<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    // The name is bytes, as in Nix: one that is not a store path name is
    // refused with the path computed from them.
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.toFile",
    )?;
    let mut ctx = StringContext::new();
    let contents = ev.force_string_ctx(
        args[1],
        &mut ctx,
        pos,
        "while evaluating the second argument passed to builtins.toFile",
    )?;
    let warn_unreferenced = |p: &str| {
        ev.host.warn(&msg!(
            "Using 'builtins.toFile' to create a file named '",
            name,
            "' that references the store path '",
            ev.config.store_dir.as_str(),
            "/",
            p,
            "' without a proper context. The resulting file will not have a correct store reference, so this is unreliable and may stop working in the future."
        ));
    };
    let mut refs: BTreeSet<StorePath> = BTreeSet::new();
    for c in &ctx {
        match c {
            ContextElem::Opaque(p) => {
                refs.insert(ev.store_path_of(p)?);
            }
            ContextElem::LazyPath(tree) => {
                // Only a known store path can be in the text.
                if let Some(p) = ev.known_lazy_path(*tree) {
                    if contents.windows(p.len()).any(|w| w == p.as_bytes()) {
                        warn_unreferenced(p);
                    }
                }
            }
            ContextElem::Path(p) => {
                if contents.windows(p.len()).any(|w| w == p.as_bytes()) {
                    warn_unreferenced(p);
                }
            }
            other => {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "files created by builtins.toFile may not reference derivations, but ",
                        name,
                        " references ",
                        other.encode()
                    ),
                    pos,
                ))
            }
        }
    }
    let sp = ev
        .store
        .borrow_mut()
        .add_text(name, contents, refs)
        .map_err(|e| ev.error_at(ErrorKind::Eval, e.message_bytes(), pos))?;
    ev.mount_store_file(&sp, contents.to_vec());
    Ok(ev.store_path_string(&sp))
}

/// `addPath`: `builtins.path` and `builtins.filterSource`. The name is
/// bytes, as in Nix: one that is not a store path name is refused with
/// the path computed from them.
#[allow(clippy::too_many_arguments)]
fn add_path<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    name: &[u8],
    p: PathVal<'a>,
    filter: Option<ValueRef<'a>>,
    method: ContentAddressMethod,
    expected_hash: Option<Hash>,
    context: &StringContext<'a>,
) -> Result<Value<'a>> {
    let r: Result<Value<'a>> = (|| {
        if p.accessor == ev.root_accessor
            && ev.maybe_store_path(p.path).is_some()
            && !context.is_empty()
        {
            ev.realise_context(context, false)?;
        }
        let expected_path = match &expected_hash {
            Some(h) => Some(
                ev.config
                    .store_dir
                    .make_fixed_output_path_from_ca(
                        name,
                        &brix_store::ContentAddressWithReferences::from_parts(
                            method,
                            *h,
                            brix_store::StoreReferences::default(),
                        )
                        .map_err(|e| ev.error(ErrorKind::Eval, e.message_bytes()))?,
                    )
                    .map_err(|e| ev.error(ErrorKind::Eval, e.message_bytes()))?,
            ),
            None => None,
        };
        if let Some(ep) = &expected_path {
            if ev.store.borrow().contains(ep) {
                return Ok(ev.store_path_string(ep));
            }
        }
        let pb = ev.bump.alloc(p);
        ev.check_path_access(pb).map_err(|e| {
            // With a filter, Nix hashes through an accessor whose
            // `maybeLstat` hides forbidden paths as missing. Unfiltered
            // copies instead report the access failure directly.
            if filter.is_some() && e.kind == ErrorKind::RestrictedPath {
                ev.error(
                    ErrorKind::FileSystem,
                    msg!("path '", ev.show_path(pb), "' does not exist"),
                )
            } else {
                e
            }
        })?;
        // `path.resolveSymlinks()`: a symlink given as the path is
        // followed, and its target copied.
        let source = ev.resolve_symlinks_fully(pb)?;
        let dst = ev.add_path_to_store(&source, name, method, filter.map(|f| (f, pos)), pos)?;
        if let Some(ep) = expected_path {
            if ep != dst {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "store path mismatch in (possibly filtered) path added from '",
                        ev.show_path(pb),
                        "'"
                    ),
                    pos,
                ));
            }
        }
        Ok(ev.store_path_string(&dst))
    })();
    r.inspect_err(|e| {
        ev.add_trace(e, pos, msg!("while adding path '", ev.show_path(&p), "'"));
    })
}

fn prim_filter_source<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let p = ev.coerce_to_path(
        pos,
        args[1],
        &mut ctx,
        "while evaluating the second argument (the path to filter) passed to 'builtins.filterSource'",
    )?;
    ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filterSource",
    )?;
    let pb = ev.bump.alloc(p);
    let name = ev.compute_base_name(pb)?;
    add_path(
        ev,
        pos,
        &name,
        p,
        Some(args[0]),
        ContentAddressMethod::NixArchive,
        None,
        &ctx,
    )
}

fn prim_path<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to 'builtins.path'",
    )?;
    let mut path: Option<PathVal<'a>> = None;
    // Bytes, as Nix's: see `add_path`.
    let mut name: &[u8] = b"";
    let mut filter: Option<ValueRef<'a>> = None;
    let mut method = ContentAddressMethod::NixArchive;
    let mut expected: Option<Hash> = None;
    let mut ctx = StringContext::new();
    for a in attrs.iter() {
        let n = ev.symbol_str(a.name);
        match n.as_str() {
            "path" => {
                path = Some(ev.coerce_to_path(
                    a.pos,
                    a.value,
                    &mut ctx,
                    "while evaluating the 'path' attribute passed to 'builtins.path'",
                )?);
            }
            "name" => {
                name = ev.force_string_no_ctx(
                    a.value,
                    a.pos,
                    "while evaluating the `name` attribute passed to builtins.path",
                )?;
            }
            "filter" => {
                ev.force_function(
                    a.value,
                    a.pos,
                    "while evaluating the `filter` parameter passed to builtins.path",
                )?;
                filter = Some(a.value);
            }
            "recursive" => {
                method = if ev.force_bool(
                    a.value,
                    a.pos,
                    "while evaluating the `recursive` attribute passed to builtins.path",
                )? {
                    ContentAddressMethod::NixArchive
                } else {
                    ContentAddressMethod::Flat
                };
            }
            "sha256" => {
                let s = ev.force_string_no_ctx(
                    a.value,
                    a.pos,
                    "while evaluating the `sha256` attribute passed to builtins.path",
                )?;
                expected = Some(new_hash_allow_empty(ev, s, Some(HashAlgo::Sha256))?);
            }
            _ => {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "unsupported argument '",
                        ev.symbol_ident(a.name),
                        "' to 'builtins.path'"
                    ),
                    a.pos,
                ))
            }
        }
    }
    let Some(p) = path else {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "missing required 'path' attribute in the first argument to 'builtins.path'",
            pos,
        ));
    };
    let base_name;
    let name = if name.is_empty() {
        base_name = source_path_base_name(ev, &p)?;
        &base_name[..]
    } else {
        name
    };
    add_path(ev, pos, name, p, filter, method, expected, &ctx)
}

/// `SourcePath::baseName`: the path's last component (bytes), `source` for
/// the root. A lazy tree's root stands for the tree's store path, whose
/// base name needs the tree's hash; every other path's last component is
/// known without it.
fn source_path_base_name<'a>(ev: &Evaluator<'a>, p: &PathVal<'a>) -> Result<BString> {
    if p.accessor == ev.root_accessor {
        if let Some(id) = ev.lazy_root(p.path) {
            return Ok(ev.materialize(id)?.as_str().into());
        }
    }
    // A path value is canonical: no trailing `/` but the root's.
    Ok(match p.path.iter().rposition(|&c| c == b'/') {
        Some(i) if i + 1 < p.path.len() => BString::from(&p.path[i + 1..]),
        _ => BString::from("source"),
    })
}

/// `newHashAllowEmpty`: an empty hash string means the zero hash (with a
/// warning), otherwise the hash is parsed as `algo` or with a prefix. The
/// hash is bytes, quoted as they are when it is not valid.
pub fn new_hash_allow_empty<'a>(
    ev: &Evaluator<'a>,
    s: &[u8],
    algo: Option<HashAlgo>,
) -> Result<Hash> {
    if s.is_empty() {
        let algo = algo.ok_or_else(|| {
            ev.error(
                ErrorKind::Eval,
                "empty hash requires explicit hash algorithm",
            )
        })?;
        let h = Hash::zero(algo);
        ev.host.warn(
            format!(
                "found empty hash, assuming '{}'",
                h.to_string(brix_store::HashFormat::Sri, true)
            )
            .as_bytes(),
        );
        return Ok(h);
    }
    Hash::parse_any(s, algo, crate::builtins::strings::xp_settings(ev))
        .map_err(|e| crate::builtins::strings::hash_error(ev, &e))
}

impl<'a> Evaluator<'a> {
    /// A string holding a store path, with that path as its context
    /// (`mkStorePathString`), and the path allowed for reading.
    pub fn store_path_string(&self, sp: &StorePath) -> Value<'a> {
        let display = self.config.store_dir.display(sp);
        self.allow_path(crate::mount::store_path_canon(&display));
        let ctx = [ContextElem::Opaque(self.arena_str(sp.as_str()))];
        self.alloc_string_value(display.as_bytes(), &ctx)
    }

    /// Adds a tree to the virtual store, optionally filtered by a Nix
    /// function `(path: type: bool)`. The name is bytes (`builtins.path`'s
    /// need not be UTF-8); one that is not a store path name is refused
    /// with Nix's message.
    pub fn add_path_to_store<N: AsRef<[u8]> + ?Sized>(
        &self,
        source: &brix_store::SourcePath,
        name: &N,
        method: ContentAddressMethod,
        filter: Option<(ValueRef<'a>, SrcPos)>,
        _pos: SrcPos,
    ) -> Result<StorePath> {
        crate::stats::Stats::bump(&self.stats.store_adds);
        let started = crate::platform::Instant::now();
        let result = self.add_path_to_store_inner(source, name.as_ref(), method, filter);
        crate::stats::Stats::add_time(&self.stats.store_time, started.elapsed());
        result
    }

    fn add_path_to_store_inner(
        &self,
        source: &brix_store::SourcePath,
        name: &[u8],
        method: ContentAddressMethod,
        filter: Option<(ValueRef<'a>, SrcPos)>,
    ) -> Result<StorePath> {
        let ingestion = brix_store::virtual_store::PathIngestion {
            method,
            algo: HashAlgo::Sha256,
            references: BTreeSet::new(),
        };
        let registry = self.accessors.borrow();
        let accessor_id = source.accessor;
        let source_accessor = registry.get(accessor_id).clone();
        let mut filter_error: Option<crate::Error> = None;
        // What the filter accepted, to show the copy as Nix would have
        // written it.
        let mut included: std::collections::HashSet<CanonPath> = std::collections::HashSet::new();
        let result = match filter {
            None => self
                .store
                .borrow_mut()
                .add_path(&registry, source, name, &ingestion, None),
            Some((f, fpos)) => {
                let accessor = registry.get(accessor_id).clone();
                // What stops the walk when evaluation failed; the error
                // itself is in `filter_error`.
                let filter_failed = || {
                    brix_store::nar::NarError::Accessor(brix_store::AccessorError::Io(
                        "filter failed".into(),
                    ))
                };
                let mut cb =
                    |p: &CanonPath| -> std::result::Result<bool, brix_store::nar::NarError> {
                        let st = accessor.lstat(p)?;
                        // The path as a string of its bytes, a lazy tree's
                        // hash left lazy (no context: Nix passes a plain
                        // string).
                        let text = if accessor_id == self.root_accessor {
                            match self.lazy_path_string(p.as_bytes(), false) {
                                Ok(text) => text,
                                Err(e) => {
                                    filter_error = Some(e);
                                    return Err(filter_failed());
                                }
                            }
                        } else {
                            self.alloc_string_value(p.as_bytes(), &[])
                        };
                        let path_v = self.alloc_cell(text);
                        let type_v =
                            self.alloc_cell(self.mk_string(file_type_string(st.file_type)));
                        let r = (|| {
                            let fv = self.force(f, fpos)?;
                            let res = self.call(fv, &[path_v, type_v], fpos)?;
                            let rc = self.alloc_cell(res);
                            self.force_bool(
                                rc,
                                fpos,
                                "while evaluating the return value of the path filter function",
                            )
                        })();
                        match r {
                            Ok(b) => {
                                if b {
                                    included.insert(p.clone());
                                }
                                Ok(b)
                            }
                            Err(e) => {
                                filter_error = Some(e);
                                Err(filter_failed())
                            }
                        }
                    };
                // The filter is Nix code, which may add paths to the store
                // (or hash a lazy tree) itself: the tree is walked without
                // the store borrowed, and the copy recorded after.
                let (walked, path) = source.resolve(&registry);
                brix_store::nar::hash_path(
                    walked,
                    path,
                    method.ingestion_method(),
                    HashAlgo::Sha256,
                    &mut cb,
                )
                .map_err(brix_store::VirtualStoreError::from)
                .and_then(|(hash, _)| {
                    self.store
                        .borrow_mut()
                        .add_hashed_path(source, name, &ingestion, hash, true)
                })
            }
        };
        drop(registry);
        match result {
            Ok(sp) => {
                // Hashing a whole lazy tree for a copy tells its own hash.
                let mut lazy_tree_path = None;
                if filter.is_none()
                    && method == ContentAddressMethod::NixArchive
                    && source.accessor == self.root_accessor
                {
                    let hash = match self.store.borrow().get(&sp) {
                        Some(brix_store::StoreObject::SourceTree { hash, .. }) => Some(*hash),
                        _ => None,
                    };
                    if let Some(hash) = hash {
                        lazy_tree_path =
                            self.note_lazy_root_hashed(source.path.as_bytes(), &hash)?;
                    }
                }
                // Nix writes the copy to the store and reads it back from
                // there; brix shows the source (filtered as the copy was) at
                // the store path instead.
                let display = self.config.store_dir.display(&sp);
                let at = crate::mount::store_path_canon(&display);
                // Copying a tree that already lives at its own store path
                // (nixpkgs' `pkgs.path` is `builtins.path` over the input
                // tree with a filter that keeps everything) is a no-op in
                // Nix: the path exists. Mounting it again would make the
                // mount point serve itself through the root accessor and
                // recurse forever on the first lookup.
                let already_there =
                    source.accessor == self.root_accessor && source.path.is_within(&at);
                if already_there {
                    self.allow_path(at);
                } else if lazy_tree_path.as_ref() == Some(&sp) {
                    // The copy is the lazy tree under its own name, which
                    // `note_lazy_root_hashed` mounted (and allowed) at its
                    // store path as the tree itself.
                } else {
                    let subtree =
                        crate::mount::SubtreeAccessor::new(source_accessor, source.path.clone());
                    let view: std::sync::Arc<dyn brix_store::SourceAccessor> = if filter.is_some() {
                        std::sync::Arc::new(crate::mount::FilteredAccessor::new(subtree, included))
                    } else {
                        std::sync::Arc::new(subtree)
                    };
                    self.mount(
                        at.clone(),
                        std::sync::Arc::new(crate::mount::DisplayPrefixAccessor::new(
                            view, display,
                        )),
                    );
                    self.allow_path(at);
                }
                Ok(sp)
            }
            Err(e) => match filter_error {
                Some(fe) => Err(fe),
                // A name's bytes as they are (`builtins.path`'s `name`).
                None => Err(self.error(ErrorKind::FileSystem, e.message_bytes())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{legacy_base_name_of, suffix_if_potential_match};

    #[test]
    fn base_names() {
        assert_eq!(legacy_base_name_of(b"/foo/bar"), b"bar");
        assert_eq!(legacy_base_name_of(b"/foo/bar/"), b"bar");
        assert_eq!(legacy_base_name_of(b"bar"), b"bar");
        assert_eq!(legacy_base_name_of(b"/"), b"");
        assert_eq!(legacy_base_name_of(b""), b"");
    }

    /// `suffixIfPotentialMatch` on bytes: a prefix matches whole path
    /// components only.
    #[test]
    fn lookup_path_prefixes() {
        let m = |prefix: &[u8], path: &'static [u8]| suffix_if_potential_match(prefix, path);
        assert_eq!(m(b"", b"a/b"), Some(&b"a/b"[..]));
        assert_eq!(m(b"p\xe9", b"p\xe9"), Some(&b""[..]));
        assert_eq!(m(b"p\xe9", b"p\xe9/caf\xe9"), Some(&b"caf\xe9"[..]));
        assert_eq!(m(b"p\xe9", b"p\xe9x/caf\xe9"), None);
        assert_eq!(m(b"p\xe9", b"p\xff/caf\xe9"), None);
        assert_eq!(m(b"p\xe9", b"p"), None);
    }
}
