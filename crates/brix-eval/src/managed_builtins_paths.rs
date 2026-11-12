//! Filesystem builtins use owned byte snapshots around rooted callbacks.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use brix_store::{
    AccessorId, CanonPath, ContentAddressMethod, FileType, Hash, HashAlgo, StorePath,
};

use crate::error::{msg, ErrorKind};
use crate::managed_bindings as bindings;
use crate::managed_builtins::{
    coerce, force_attrs, force_bool, force_function, force_list, force_string, force_string_no_ctx,
};
use crate::managed_builtins_strings::{self as strings, OwnedContext};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{CellRoot, RootedAttr, Roots, ValueRoot, ValueView};
use crate::managed_language::Language;
use crate::symbol::sym;
use crate::value::{PathVal, SrcPos};
use crate::Result;

pub(crate) fn coerce_path<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    _roots: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<(AccessorId, Vec<u8>, Vec<OwnedContext>)> {
    l.legacy().with_stack(|| {
        m.with_roots(|m, r| {
            let value = l
                .force(m, r, cell, pos)
                .map_err(|e| l.legacy().traced(e, pos, ctx))?;
            match m.heap.view(value) {
                ValueView::Path(path) => {
                    return Ok((path.accessor(), path.bytes().to_vec(), Vec::new()))
                }
                ValueView::Attrs(attrs) => {
                    if let Some(to_string) = attrs.get(sym::TO_STRING) {
                        let fun = to_string.value.root(r);
                        let fun = l.force(m, r, fun, pos)?;
                        let value = l.call_value(m, r, fun, &[cell], pos)?;
                        let cell = allocation(m.heap.cell(r, value))?;
                        return coerce_path(l, m, r, cell, pos, ctx);
                    }
                }
                ValueView::String(_) => {
                    let (bytes, mut context) = strings::snapshot(m, value);
                    let hashes: Vec<_> = context
                        .iter()
                        .filter_map(|c| match c {
                            OwnedContext::LazyHash { offset, tree } => {
                                Some(crate::lazy_tree::LazyHash {
                                    offset: *offset as usize,
                                    tree: *tree,
                                })
                            }
                            _ => None,
                        })
                        .collect();
                    if let [hash] = hashes.as_slice() {
                        if let Some(path) = l.legacy().lazy_text_path(&bytes, *hash) {
                            context.retain(|c| !matches!(c, OwnedContext::LazyHash { .. }));
                            let path = l.legacy().canon_path(&path)?;
                            return Ok((
                                l.legacy().root_accessor,
                                path.as_bytes().to_vec(),
                                context,
                            ));
                        }
                    }
                }
                _ => {}
            }
            let value = coerce(l, m, r, cell, pos, false, false)
                .map_err(|e| l.legacy().traced(e, pos, ctx))?;
            let value = strings::materialize(l, m, r, value)?;
            let (bytes, context) = strings::snapshot(m, value);
            if !bytes.starts_with(b"/") {
                return Err(l
                    .legacy()
                    .error(
                        ErrorKind::Eval,
                        msg!("string '", bytes, "' doesn't represent an absolute path"),
                    )
                    .with_trace(l.legacy().position(pos), ctx));
            }
            let path = l.legacy().canon_path(&bytes)?;
            Ok((l.legacy().root_accessor, path.as_bytes().to_vec(), context))
        })
    })
}

pub(crate) fn realise_path<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<(AccessorId, Vec<u8>)> {
    let (accessor, path, context) = coerce_path(
        l,
        m,
        roots,
        cell,
        SrcPos::NONE,
        "while realising the context of a path",
    )?;
    if accessor == l.legacy().root_accessor && !context.is_empty() {
        if strings::needs_build(&context) {
            crate::runtime::ordered_effect()?;
        }
        m.heap
            .blocking(|| {
                l.legacy()
                    .realise_context(&strings::borrowed_context(&context)?, true)
            })
            .inspect_err(|e| {
                l.legacy().add_trace(
                    e,
                    pos,
                    msg!(
                        "while realising the context of path '",
                        l.legacy().show_path(&PathVal {
                            accessor,
                            path: &path
                        }),
                        "'"
                    ),
                );
            })?;
    }
    Ok((accessor, path))
}

pub(crate) fn lazy_path_string<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    path: &[u8],
    with_context: bool,
) -> Result<ValueRoot<'r, 'h>> {
    if let Some((text, offset, tree)) = l.legacy().lazy_path_text(path) {
        let mut context = vec![OwnedContext::LazyHash {
            offset: offset as u32,
            tree,
        }];
        if with_context {
            context.push(OwnedContext::LazyPath(tree));
        }
        return strings::string(m, r, &text, &context);
    }
    let text = l.legacy().logical_path(path)?;
    let context = if with_context {
        l.legacy()
            .maybe_store_path(&text)
            .map(|p| OwnedContext::Path(p.as_str().as_bytes().to_vec()))
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    strings::string(m, r, &text, &context)
}

pub(crate) fn store_path_string<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    path: &StorePath,
) -> Result<ValueRoot<'r, 'h>> {
    let text = l.legacy().config.store_dir.display(path);
    l.legacy().allow_path(crate::mount::store_path_canon(&text));
    strings::string(
        m,
        r,
        text.as_bytes(),
        &[OwnedContext::Opaque(path.as_str().as_bytes().to_vec())],
    )
}

pub(crate) fn call<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Option<Result<ValueRoot<'r, 'h>>> {
    let handler = resolve(name)?;
    Some(handler(
        l,
        m,
        roots,
        name.strip_prefix("__").unwrap_or(name),
        args,
        pos,
    ))
}

pub(crate) fn resolve(name: &str) -> Option<super::managed_builtins::Handler> {
    let name = name.strip_prefix("__").unwrap_or(name);
    if !matches!(
        name,
        "placeholder"
            | "toPath"
            | "storePath"
            | "pathExists"
            | "baseNameOf"
            | "dirOf"
            | "readFile"
            | "findFile"
            | "hashFile"
            | "readFileType"
            | "readDir"
            | "toFile"
            | "filterSource"
            | "path"
    ) {
        return None;
    }
    Some(dispatch)
}

fn dispatch<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    name: &str,
    a: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    match name {
        "placeholder" => {
            let name = force_string_no_ctx(
                l,
                m,
                r,
                a[0],
                pos,
                "while evaluating the first argument passed to builtins.placeholder",
            )?;
            allocation(m.heap.string(
                r,
                brix_store::derivation::hash_placeholder(&name).as_bytes(),
            ))
        }
        "storePath" => Err(ev.error_at(
            ErrorKind::Eval,
            "'builtins.storePath' is not allowed in pure evaluation mode",
            pos,
        )),
        "toPath" => {
            let (accessor, path, context) = coerce_path(
                l,
                m,
                r,
                a[0],
                pos,
                "while evaluating the first argument passed to builtins.toPath",
            )?;
            let text = ev.path_text(&PathVal {
                accessor,
                path: &path,
            })?;
            strings::string(m, r, &text, &context)
        }
        "pathExists" => {
            let value = l.force(m, r, a[0], pos)?;
            let must_be_dir = matches!(m.heap.view(value), ValueView::String(s) if s.bytes().ends_with(b"/") || s.bytes().ends_with(b"/."));
            let result: Result<bool> = (|| {
                let (accessor, path) = realise_path(l, m, r, a[0], pos)?;
                let stat = ev.lstat(
                    &PathVal {
                        accessor,
                        path: &path,
                    },
                    must_be_dir,
                )?;
                Ok(stat.is_some_and(|s| !must_be_dir || s.file_type == FileType::Directory))
            })();
            match result {
                Ok(found) => Ok(r.bool(found)),
                Err(e) if e.kind == ErrorKind::RestrictedPath => Ok(r.bool(false)),
                Err(e) => Err(e),
            }
        }
        "baseNameOf" | "dirOf" => path_component(l, m, r, a[0], pos, name == "dirOf"),
        "findFile" => find_file(l, m, r, a, pos),
        "toFile" => to_file(l, m, r, a, pos),
        "path" | "filterSource" => path_builtin(l, m, r, a, pos, name == "filterSource"),
        "hashFile" => {
            let algorithm = force_string_no_ctx(
                l,
                m,
                r,
                a[0],
                pos,
                "while evaluating the first argument passed to builtins.hashFile",
            )?;
            let algorithm = crate::builtins::strings::parse_hash_algo(ev, &algorithm, pos)?;
            let (accessor, path) = realise_path(l, m, r, a[1], pos)?;
            let contents = ev.read_file(&PathVal {
                accessor,
                path: &path,
            })?;
            allocation(
                m.heap.string(
                    r,
                    Hash::of(algorithm, &contents)
                        .to_string(brix_store::HashFormat::Base16, false)
                        .as_bytes(),
                ),
            )
        }
        "readFileType" => {
            let (accessor, path) = realise_path(l, m, r, a[0], pos)?;
            let stat = ev.lstat_required(&PathVal {
                accessor,
                path: &path,
            })?;
            allocation(m.heap.string(r, file_type(stat.file_type).as_bytes()))
        }
        "readFile" => {
            let (accessor, path) = realise_path(l, m, r, a[0], pos)?;
            let p = PathVal {
                accessor,
                path: &path,
            };
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
            let mut context = Vec::new();
            if accessor == ev.root_accessor {
                if let Some(path) = ev.maybe_store_path(&path) {
                    let refs: BTreeSet<StorePath> = ev
                        .store
                        .borrow()
                        .get(&path)
                        .map(|obj| match obj {
                            brix_store::StoreObject::Text { references, .. } => references.clone(),
                            brix_store::StoreObject::Derivation(d) => d.references(),
                            brix_store::StoreObject::SourceTree { .. } => BTreeSet::new(),
                        })
                        .unwrap_or_default();
                    for path in refs {
                        if contents
                            .windows(path.hash_part().len())
                            .any(|w| w == path.hash_part().as_bytes())
                        {
                            context.push(OwnedContext::Opaque(path.as_str().as_bytes().to_vec()));
                        }
                    }
                }
            }
            strings::string(m, r, &contents, &context)
        }
        "readDir" => {
            let (accessor, path) = realise_path(l, m, r, a[0], pos)?;
            let entries = ev.read_dir(&PathVal {
                accessor,
                path: &path,
            })?;
            let dir = &path;
            let mut len = dir.len();
            while len > 0 && dir[len - 1] == b'/' {
                len -= 1;
            }
            let mut attrs = Vec::with_capacity(entries.len());
            for (name, ty) in entries {
                let value = m.with_roots(|m, t| {
                    let value = match ty {
                        Some(ty) => allocation(m.heap.string(t, file_type(ty).as_bytes()))?,
                        None => {
                            let mut child = path[..len].to_vec();
                            child.push(b'/');
                            child.extend_from_slice(&name);
                            let value = allocation(m.heap.path(t, accessor, &child))?;
                            let child = allocation(m.heap.cell(t, value))?;
                            let fun = l.builtin(m, t, "readFileType")?;
                            allocation(m.heap.app(t, fun, child, false))?
                        }
                    };
                    let cell = allocation(m.heap.cell(t, value))?;
                    Ok::<_, crate::Error>(r.copy(cell))
                })?;
                attrs.push(RootedAttr {
                    name: ev.intern(&name),
                    pos: SrcPos::NONE,
                    value,
                });
            }
            attrs_value(m, r, attrs)
        }
        _ => unreachable!(),
    }
}

fn attrs_value<'r, 'h>(
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    mut attrs: Vec<RootedAttr<'_, 'h>>,
) -> Result<ValueRoot<'r, 'h>> {
    bindings::sort_dedup(&mut attrs);
    let set = allocation(bindings::from_sorted(&mut m.heap, r, &attrs, SrcPos::NONE))?;
    Ok(m.heap.bindings_value(r, set))
}

fn file_type(ty: FileType) -> &'static str {
    match ty {
        FileType::Regular => "regular",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        _ => "unknown",
    }
}

fn path_component<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    parent: bool,
) -> Result<ValueRoot<'r, 'h>> {
    let value = if parent {
        l.force(m, r, cell, pos)?
    } else {
        let _guard = l.legacy().enter(pos)?;
        l.force(m, r, cell, pos)?
    };
    if parent {
        if let ValueView::Path(path) = m.heap.view(value) {
            let accessor = path.accessor();
            let bytes = path.bytes();
            let end = bytes.iter().rposition(|b| *b == b'/').unwrap_or(0).max(1);
            let bytes = bytes[..end].to_vec();
            return allocation(m.heap.path(r, accessor, &bytes));
        }
    }
    let ctx = if parent {
        "while evaluating the first argument passed to 'builtins.dirOf'"
    } else {
        "while evaluating the first argument passed to builtins.baseNameOf"
    };
    let value =
        crate::managed_builtins::coerce_mode(l, m, r, cell, pos, crate::coerce::Coercion::RAW)
            .map_err(|e| l.legacy().traced(e, pos, ctx))?;
    let (bytes, mut context) = strings::snapshot(m, value);
    let (start, end) = if parent {
        match bytes.iter().rposition(|b| *b == b'/') {
            None | Some(0) => {
                context.retain(|c| !matches!(c, OwnedContext::LazyHash { .. }));
                return strings::string(
                    m,
                    r,
                    if bytes.starts_with(b"/") { b"/" } else { b"." },
                    &context,
                );
            }
            Some(end) => (0, end),
        }
    } else {
        if bytes.is_empty() {
            return Ok(value);
        }
        let mut last = bytes.len() - 1;
        if bytes[last] == b'/' && last > 0 {
            last -= 1;
        }
        let start = bytes[..=last]
            .iter()
            .rposition(|b| *b == b'/')
            .map_or(0, |i| i + 1);
        (start, (last + 1).max(start))
    };
    // Slash cuts cannot split a hash. Keep hidden path context, reposition
    // hashes wholly within the selected substring, discard other text hashes.
    context.retain_mut(|c| match c {
        OwnedContext::LazyHash { offset, .. } => {
            let at = *offset as usize;
            if at >= start && at + crate::lazy_tree::HASH_LEN <= end {
                *offset -= start as u32;
                true
            } else {
                false
            }
        }
        _ => true,
    });
    strings::string(m, r, &bytes[start..end], &context)
}

fn find_file<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    a: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let input = force_list(
        l,
        m,
        r,
        a[0],
        pos,
        "while evaluating the first argument passed to builtins.findFile",
    )?;
    let ValueView::List(list) = m.heap.view(input) else {
        unreachable!()
    };
    let len = list.len();
    let mut lookup = Vec::with_capacity(len);
    for index in 0..len {
        lookup.push(m.with_roots(|m, t| {
            let ValueView::List(list) = m.heap.view(input) else { unreachable!() };
            let cell = list.get(index).unwrap().root(t);
            let attrs = force_attrs(l, m, t, cell, pos, "while evaluating an element of the list passed to builtins.findFile")?;
            let prefix = match m.heap.bindings_view(attrs).get(sym::PREFIX).map(|v| v.value.root(t)) {
                Some(cell) => force_string_no_ctx(l, m, t, cell, pos, "while evaluating the `prefix` attribute of an element of the list passed to builtins.findFile")?,
                None => Vec::new(),
            };
            let path = m.heap.bindings_view(attrs).get(sym::PATH).map(|a| a.value.root(t)).ok_or_else(|| ev.error(ErrorKind::TypeError, "attribute 'path' missing").with_trace(None, "in an element of the __nixPath"))?;
            let path = coerce(l, m, t, path, pos, false, false).map_err(|e| ev.traced(e, pos, "while evaluating the `path` attribute of an element of the list passed to builtins.findFile"))?;
            let path = strings::materialize(l, m, t, path)?;
            let (path, context) = strings::snapshot(m, path);
            if strings::needs_build(&context) { crate::runtime::ordered_effect()?; }
            m.heap.blocking(|| ev.realise_context(&strings::borrowed_context(&context)?, false)).map_err(|e| match e.invalid_path.get() {
                Some(invalid) => ev.error_at(ErrorKind::Eval, msg!("cannot find '", path, "', since path '", invalid.as_str(), "' is not valid"), pos), None => e,
            })?;
            Ok::<_, crate::Error>((prefix, path))
        })?);
    }
    let name = force_string_no_ctx(
        l,
        m,
        r,
        a[1],
        pos,
        "while evaluating the second argument passed to builtins.findFile",
    )?;
    for (prefix, path) in &lookup {
        let Some(rest) = name.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        let suffix = if !prefix.is_empty() && !rest.is_empty() {
            match rest.strip_prefix(b"/") {
                Some(rest) => rest,
                None => continue,
            }
        } else {
            rest
        };
        // Preserve the existing host's absolute-only lookup policy.
        if !path.starts_with(b"/") {
            continue;
        }
        let base = ev.canon_path(path)?;
        if ev
            .lstat(
                &PathVal {
                    accessor: ev.root_accessor,
                    path: base.as_bytes(),
                },
                true,
            )?
            .is_none()
        {
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
        let full = base.join(&ev.canon_path(suffix)?);
        if ev
            .lstat(
                &PathVal {
                    accessor: ev.root_accessor,
                    path: full.as_bytes(),
                },
                true,
            )?
            .is_some()
        {
            return allocation(m.heap.path(r, ev.root_accessor, full.as_bytes()));
        }
    }
    if let Some(name) = name.strip_prefix(b"nix/") {
        let accessor = ev.corepkgs_accessor();
        let path = ev.canon_path(name)?;
        return allocation(m.heap.path(r, accessor, path.as_bytes()));
    }
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

fn to_file<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    a: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let name = force_string_no_ctx(
        l,
        m,
        r,
        a[0],
        pos,
        "while evaluating the first argument passed to builtins.toFile",
    )?;
    let contents = force_string(
        l,
        m,
        r,
        a[1],
        pos,
        "while evaluating the second argument passed to builtins.toFile",
    )?;
    let contents = strings::materialize(l, m, r, contents)?;
    let (contents, context) = strings::snapshot(m, contents);
    let context = strings::borrowed_context(&context)?;
    let warn = |path: &str| -> Result<()> {
        crate::runtime::ordered_effect()?;
        ev.host.warn(&msg!("Using 'builtins.toFile' to create a file named '", name, "' that references the store path '", ev.config.store_dir.as_str(), "/", path, "' without a proper context. The resulting file will not have a correct store reference, so this is unreliable and may stop working in the future."));
        Ok(())
    };
    let mut refs = BTreeSet::new();
    for elem in &context {
        match elem {
            crate::value::ContextElem::Opaque(path) => {
                refs.insert(ev.store_path_of(path)?);
            }
            crate::value::ContextElem::LazyPath(tree) => {
                if let Some(path) = ev.known_lazy_path(*tree) {
                    if contents.windows(path.len()).any(|w| w == path.as_bytes()) {
                        warn(path)?;
                    }
                }
            }
            crate::value::ContextElem::Path(path) => {
                if contents.windows(path.len()).any(|w| w == path.as_bytes()) {
                    warn(path)?;
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
    let path = ev
        .store
        .borrow_mut()
        .add_text(&name, &contents, refs)
        .map_err(|e| ev.error_at(ErrorKind::Eval, e.message_bytes(), pos))?;
    ev.mount_store_file(&path, contents);
    store_path_string(l, m, r, &path)
}

fn path_builtin<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    a: &[CellRoot<'_, 'h>],
    pos: SrcPos,
    filter_source: bool,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    if filter_source {
        let (accessor, path, context) = coerce_path(l, m, r, a[1], pos, "while evaluating the second argument (the path to filter) passed to 'builtins.filterSource'")?;
        force_function(
            l,
            m,
            r,
            a[0],
            pos,
            "while evaluating the first argument passed to builtins.filterSource",
        )?;
        let p = PathVal {
            accessor,
            path: &path,
        };
        let name = ev.compute_base_name(&p)?;
        return add_path(
            l,
            m,
            r,
            pos,
            &name,
            &p,
            Some(a[0]),
            ContentAddressMethod::NixArchive,
            None,
            &context,
        );
    }
    let set = force_attrs(
        l,
        m,
        r,
        a[0],
        pos,
        "while evaluating the argument passed to 'builtins.path'",
    )?;
    let attrs = bindings::attributes(&m.heap, r, set);
    let mut path = None;
    let mut name = Vec::new();
    let mut filter = None;
    let mut method = ContentAddressMethod::NixArchive;
    let mut expected = None;
    for attr in attrs {
        match ev.symbol_name(attr.name) {
            b"path" => {
                path = Some(coerce_path(
                    l,
                    m,
                    r,
                    attr.value,
                    attr.pos,
                    "while evaluating the 'path' attribute passed to 'builtins.path'",
                )?)
            }
            b"name" => {
                name = force_string_no_ctx(
                    l,
                    m,
                    r,
                    attr.value,
                    attr.pos,
                    "while evaluating the `name` attribute passed to builtins.path",
                )?
            }
            b"filter" => {
                force_function(
                    l,
                    m,
                    r,
                    attr.value,
                    attr.pos,
                    "while evaluating the `filter` parameter passed to builtins.path",
                )?;
                filter = Some(attr.value);
            }
            b"recursive" => {
                method = if force_bool(
                    l,
                    m,
                    r,
                    attr.value,
                    attr.pos,
                    "while evaluating the `recursive` attribute passed to builtins.path",
                )? {
                    ContentAddressMethod::NixArchive
                } else {
                    ContentAddressMethod::Flat
                }
            }
            b"sha256" => {
                let hash = force_string_no_ctx(
                    l,
                    m,
                    r,
                    attr.value,
                    attr.pos,
                    "while evaluating the `sha256` attribute passed to builtins.path",
                )?;
                expected = Some(crate::builtins::paths::new_hash_allow_empty(
                    ev,
                    &hash,
                    Some(HashAlgo::Sha256),
                )?);
            }
            _ => {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "unsupported argument '",
                        ev.symbol_ident(attr.name),
                        "' to 'builtins.path'"
                    ),
                    attr.pos,
                ))
            }
        }
    }
    let Some((accessor, path, context)) = path else {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "missing required 'path' attribute in the first argument to 'builtins.path'",
            pos,
        ));
    };
    if name.is_empty() {
        if accessor == ev.root_accessor {
            if let Some(id) = ev.lazy_root(&path) {
                name = ev.materialize(id)?.as_str().as_bytes().to_vec();
            }
        }
        if name.is_empty() {
            name = match path.iter().rposition(|b| *b == b'/') {
                Some(i) if i + 1 < path.len() => path[i + 1..].to_vec(),
                _ => b"source".to_vec(),
            };
        }
    }
    add_path(
        l,
        m,
        r,
        pos,
        &name,
        &PathVal {
            accessor,
            path: &path,
        },
        filter,
        method,
        expected,
        &context,
    )
}

#[allow(clippy::too_many_arguments)]
fn add_path<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    pos: SrcPos,
    name: &[u8],
    path: &PathVal<'_>,
    filter: Option<CellRoot<'_, 'h>>,
    method: ContentAddressMethod,
    expected_hash: Option<Hash>,
    context: &[OwnedContext],
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let result = (|| {
        if path.accessor == ev.root_accessor
            && ev.maybe_store_path(path.path).is_some()
            && !context.is_empty()
        {
            if strings::needs_build(context) {
                crate::runtime::ordered_effect()?;
            }
            m.heap
                .blocking(|| ev.realise_context(&strings::borrowed_context(context)?, false))?;
        }
        let expected_path = match expected_hash {
            Some(hash) => Some(
                ev.config
                    .store_dir
                    .make_fixed_output_path_from_ca(
                        name,
                        &brix_store::ContentAddressWithReferences::from_parts(
                            method,
                            hash,
                            brix_store::StoreReferences::default(),
                        )
                        .map_err(|e| ev.error(ErrorKind::Eval, e.message_bytes()))?,
                    )
                    .map_err(|e| ev.error(ErrorKind::Eval, e.message_bytes()))?,
            ),
            None => None,
        };
        if let Some(expected) = &expected_path {
            if ev.store.borrow().contains(expected) {
                return store_path_string(l, m, r, expected);
            }
        }
        ev.check_path_access(path).map_err(|e| {
            if filter.is_some() && e.kind == ErrorKind::RestrictedPath {
                ev.error(
                    ErrorKind::FileSystem,
                    msg!("path '", ev.show_path(path), "' does not exist"),
                )
            } else {
                e
            }
        })?;
        let source = ev.resolve_symlinks_fully(path)?;
        let destination = add_path_to_store(l, m, r, &source, name, method, filter, pos)?;
        if expected_path.is_some_and(|expected| expected != destination) {
            return Err(ev.error_at(
                ErrorKind::Eval,
                msg!(
                    "store path mismatch in (possibly filtered) path added from '",
                    ev.show_path(path),
                    "'"
                ),
                pos,
            ));
        }
        store_path_string(l, m, r, &destination)
    })();
    result
        .inspect_err(|e| ev.add_trace(e, pos, msg!("while adding path '", ev.show_path(path), "'")))
}

#[allow(clippy::too_many_arguments)]
fn add_path_to_store<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    _r: &Roots<'_, 'h>,
    source: &brix_store::SourcePath,
    name: &[u8],
    method: ContentAddressMethod,
    filter: Option<CellRoot<'_, 'h>>,
    pos: SrcPos,
) -> Result<StorePath> {
    let ev = l.legacy();
    let Some(filter) = filter else {
        // This branch performs ingestion only and never evaluates a value.
        return m
            .heap
            .blocking(|| ev.add_path_to_store(source, name, method, None, pos));
    };
    crate::stats::Stats::bump(&ev.stats.store_adds);
    let started = crate::platform::Instant::now();
    let result = (|| {
        let ingestion = brix_store::virtual_store::PathIngestion {
            method,
            algo: HashAlgo::Sha256,
            references: BTreeSet::new(),
        };
        // Clone the accessor before running Nix callbacks: callbacks may mount
        // new files or ingest other paths, so no registry/store borrow survives.
        let accessor = ev.accessor(source.accessor);
        let mut included: HashSet<CanonPath> = HashSet::new();
        let mut filter_error = None;
        let mut callback =
            |path: &CanonPath| -> std::result::Result<bool, brix_store::nar::NarError> {
                let stat = accessor.lstat(path)?;
                let result = m.with_roots(|m, t| {
                    let text = if source.accessor == ev.root_accessor {
                        lazy_path_string(l, m, t, path.as_bytes(), false)?
                    } else {
                        allocation(m.heap.string(t, path.as_bytes()))?
                    };
                    let path_arg = allocation(m.heap.cell(t, text))?;
                    let kind = allocation(m.heap.string(t, file_type(stat.file_type).as_bytes()))?;
                    let type_arg = allocation(m.heap.cell(t, kind))?;
                    let fun = l.force(m, t, filter, pos)?;
                    let result = l.call_value(m, t, fun, &[path_arg, type_arg], pos)?;
                    let result = allocation(m.heap.cell(t, result))?;
                    force_bool(
                        l,
                        m,
                        t,
                        result,
                        pos,
                        "while evaluating the return value of the path filter function",
                    )
                });
                match result {
                    Ok(accept) => {
                        if accept {
                            included.insert(path.clone());
                        }
                        Ok(accept)
                    }
                    Err(error) => {
                        filter_error = Some(error);
                        Err(brix_store::nar::NarError::Accessor(
                            brix_store::AccessorError::Io("filter failed".into()),
                        ))
                    }
                }
            };
        let hashed = brix_store::nar::hash_path(
            accessor.as_ref(),
            &source.path,
            method.ingestion_method(),
            HashAlgo::Sha256,
            &mut callback,
        );
        let (hash, _) = match hashed {
            Ok(hash) => hash,
            Err(error) => {
                return Err(filter_error.unwrap_or_else(|| {
                    ev.error(
                        ErrorKind::FileSystem,
                        brix_store::VirtualStoreError::from(error).message_bytes(),
                    )
                }))
            }
        };
        let path = ev
            .store
            .borrow_mut()
            .add_hashed_path(source, name, &ingestion, hash, true)
            .map_err(|e| ev.error(ErrorKind::FileSystem, e.message_bytes()))?;
        let display = ev.config.store_dir.display(&path);
        let at = crate::mount::store_path_canon(&display);
        if source.accessor == ev.root_accessor && source.path.is_within(&at) {
            ev.allow_path(at);
        } else {
            let subtree = crate::mount::SubtreeAccessor::new(accessor, source.path.clone());
            let view = Arc::new(crate::mount::FilteredAccessor::new(subtree, included));
            ev.mount(
                at.clone(),
                Arc::new(crate::mount::DisplayPrefixAccessor::new(view, display)),
            );
            ev.allow_path(at);
        }
        Ok(path)
    })();
    crate::stats::Stats::add_time(&ev.stats.store_time, started.elapsed());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;

    fn cases(expressions: &[&[u8]]) {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let ev = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let mut files = brix_store::MemoryAccessor::new();
        for (path, text) in [
            (&b"/keep"[..], &b"hello"[..]),
            (b"/drop", b"bye"),
            (b"/sub/f", b"inside"),
            (b"/caf\xff", b"bytes"),
            (b"/nul", b"a\0b"),
        ] {
            files
                .add_file(&CanonPath::new(path).unwrap(), text, false)
                .unwrap();
        }
        files
            .add_symlink(&CanonPath::new("/link").unwrap(), b"keep")
            .unwrap();
        ev.mount(CanonPath::new("/fixtures").unwrap(), Arc::new(files));
        ev.allow_path(CanonPath::new("/fixtures").unwrap());
        let language = Language::new(ev);
        for stress in [false, true] {
            with_owned_heap(
                brix_gc::Config {
                    workers: 0,
                    chunk_bytes: 256,
                    steady_threshold_bytes: None,
                    trace_collections: false,
                    threshold_bytes: 1024,
                    stress,
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    for source in expressions {
                        machine.with_roots(|machine, roots| {
                            let value = language
                                .eval_str(machine, roots, source, "/fixtures")
                                .unwrap_or_else(|error| {
                                    panic!(
                                        "{}: {}",
                                        String::from_utf8_lossy(source),
                                        error.render(true)
                                    )
                                });
                            assert!(
                                matches!(machine.heap.view(value), ValueView::Bool(true)),
                                "{}",
                                String::from_utf8_lossy(source)
                            );
                            machine.heap.collect().unwrap();
                            assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                        });
                    }
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn managed_path_values_and_files_preserve_bytes_context_and_laziness() {
        cases(&[
            b"builtins.toPath \"/x/../y\" == \"/y\"",
            b"baseNameOf \"/a/b/\" == \"b\" && baseNameOf \"/\" == \"\"",
            b"baseNameOf (/a + \"/caf\xff\") == \"caf\xff\"",
            b"dirOf (/a + \"/caf\xff\") == /a",
            b"dirOf \"x\" == \".\" && dirOf \"/x\" == \"/\" && dirOf /. == /.",
            b"builtins.pathExists /fixtures/keep && !(builtins.pathExists /forbidden)",
            b"!(builtins.pathExists \"/fixtures/keep/\") && builtins.pathExists \"/fixtures/sub/.\"",
            b"builtins.readFile /fixtures/keep == \"hello\"",
            b"builtins.readFile /fixtures/link == \"hello\"",
            b"builtins.readFile (/fixtures + \"/caf\xff\") == \"bytes\"",
            b"builtins.readFileType /fixtures/link == \"symlink\"",
            b"(builtins.readDir /fixtures).sub == \"directory\" && (builtins.readDir /fixtures).keep == \"regular\"",
            b"builtins.hasAttr \"caf\xff\" (builtins.readDir /fixtures)",
            b"builtins.hashFile \"sha256\" /fixtures/keep == builtins.hashString \"sha256\" \"hello\"",
            b"builtins.findFile [ { prefix = \"p\"; path = /fixtures; } ] \"p/keep\" == /fixtures/keep",
            b"builtins.readFile (builtins.toFile \"made\" \"hello\") == \"hello\"",
            b"builtins.hasContext (builtins.toFile \"made\" \"hello\")",
            b"builtins.stringLength (builtins.placeholder \"out\") > 0",
        ]);
    }

    #[test]
    fn managed_filtered_copies_allow_collecting_callbacks_and_nested_store_writes() {
        cases(&[
            b"let p = builtins.path { path = /fixtures; name = \"copy\"; filter = p: t: baseNameOf p != \"drop\"; }; in builtins.readFile (p + \"/keep\") == \"hello\" && !(builtins.pathExists (p + \"/drop\"))",
            b"let p = builtins.filterSource (p: t: builtins.seq (builtins.toFile \"nested\" (baseNameOf p)) (baseNameOf p != \"drop\")) /fixtures; in builtins.readFile (p + \"/sub/f\") == \"inside\" && !(builtins.pathExists (p + \"/drop\"))",
            b"let p = builtins.path { path = /fixtures/keep; name = \"flat\"; recursive = false; }; in builtins.readFile p == \"hello\"",
            b"builtins.readFile (builtins.filterSource (p: t: true) /fixtures/link) == \"hello\"",
        ]);
    }
    #[test]
    fn managed_lazy_tree_paths_do_not_hash_for_components_or_reads() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let ev = crate::Evaluator::new(&arena, crate::Config::default(), &host);
        let mut files = brix_store::MemoryAccessor::new();
        files
            .add_file(&CanonPath::new("/sub/file").unwrap(), b"content", false)
            .unwrap();
        let (id, path) = ev.mount_lazy_tree(Arc::new(files), "source", "/checkout");
        let language = Language::new(ev);
        with_owned_heap(
            brix_gc::Config {
                workers: 0,
                chunk_bytes: 256,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 1024,
                stress: true,
            },
            |heap| {
                let mut machine = Machine::new(heap);
                for template in [
                    "baseNameOf (toString PATH/sub/file) == \"file\"",
                    "baseNameOf (dirOf (toString PATH/sub/file)) == \"sub\"",
                    "builtins.readFile (toString PATH/sub/file) == \"content\"",
                    "builtins.pathExists (toString PATH/sub/file)",
                ] {
                    let source =
                        template.replace("PATH", std::str::from_utf8(path.as_bytes()).unwrap());
                    machine.with_roots(|machine, roots| {
                        let value = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .unwrap_or_else(|e| panic!("{source}: {}", e.render(true)));
                        assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                        machine.heap.collect().unwrap();
                    });
                    assert!(
                        language.legacy().known_lazy_path(id.0).is_none(),
                        "{source} hashed the whole tree"
                    );
                }
            },
        )
        .unwrap();
    }
}
