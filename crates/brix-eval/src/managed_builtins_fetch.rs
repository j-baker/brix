//! Fetcher argument forcing and result construction on the moving heap.

use crate::builtins::fetch::{check_nar_hash_bytes, fetch_failed, fetch_text, mount_tree};
use crate::builtins::paths::new_hash_allow_empty;
use crate::error::{msg, ErrorKind};
use crate::host::{FetchAttr, FetchRequest};
use crate::managed_bindings as bindings;
use crate::managed_builtins::{coerce, force_string_no_ctx};
use crate::managed_builtins_paths::store_path_string;
use crate::managed_builtins_strings as strings;
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{CellRoot, RootedAttr, Roots, ValueRoot, ValueView};
use crate::managed_language::Language;
use crate::symbol::sym;
use crate::value::SrcPos;
use crate::Result;
use brix_store::{
    ContentAddressMethod, FileIngestionMethod, FixedOutputInfo, Hash, HashAlgo, HashFormat,
    PathIngestion, SourcePath, StorePath,
};
use std::collections::BTreeMap;

pub(crate) fn call<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Option<Result<ValueRoot<'r, 'h>>> {
    Some(resolve(name)?(
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
        "fetchTree" | "fetchGit" | "fetchurl" | "fetchTarball" | "fetchMercurial" | "fetchClosure"
    ) {
        return None;
    }
    Some(dispatch)
}

fn dispatch<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    // Language::apply owns the bounded root scope for this application. Nested
    // argument loops retain their own scopes, including publicKeys serialization.
    match name {
        "fetchTree" | "fetchGit" => fetch_tree(l, m, roots, args[0], pos, name == "fetchGit"),
        "fetchurl" => fetch(l, m, roots, args[0], pos, "fetchurl", false, ""),
        "fetchTarball" => fetch(l, m, roots, args[0], pos, "fetchTarball", true, "source"),
        "fetchMercurial" => mercurial(l, m, roots, args[0], pos),
        "fetchClosure" => Err(l.legacy().error_at(
            ErrorKind::Unsupported,
            "brix does not implement 'builtins.fetchClosure'",
            pos,
        )),
        _ => unreachable!(),
    }
}

fn coerced<'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'_, 'h>,
    cell: CellRoot<'_, 'h>,
    pos: SrcPos,
    ctx: &str,
) -> Result<Vec<u8>> {
    let value =
        coerce(l, m, r, cell, pos, false, false).map_err(|e| l.legacy().traced(e, pos, ctx))?;
    let value = strings::materialize(l, m, r, value)?;
    Ok(strings::snapshot(m, value).0)
}

fn emit<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    path: &StorePath,
    attrs: &BTreeMap<String, FetchAttr>,
) -> Result<ValueRoot<'r, 'h>> {
    let value = store_path_string(l, m, r, path)?;
    let mut result = vec![RootedAttr {
        name: sym::OUT_PATH,
        pos: SrcPos::NONE,
        value: allocation(m.heap.cell(r, value))?,
    }];
    for (name, value) in attrs {
        let cell = m.with_roots(|m, t| {
            let value = match value {
                FetchAttr::String(bytes) => allocation(m.heap.string(t, bytes.as_bytes()))?,
                FetchAttr::Int(n) => t.int(*n as i64),
                FetchAttr::Bool(b) => t.bool(*b),
            };
            let cell = allocation(m.heap.cell(t, value))?;
            Ok::<_, crate::Error>(r.copy(cell))
        })?;
        result.push(RootedAttr {
            name: l.legacy().sym(name),
            pos: SrcPos::NONE,
            value: cell,
        });
    }
    bindings::sort_dedup(&mut result);
    let attrs = allocation(bindings::from_sorted(&mut m.heap, r, &result, SrcPos::NONE))?;
    Ok(m.heap.bindings_value(r, attrs))
}

fn fetch_tree<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    arg: CellRoot<'_, 'h>,
    pos: SrcPos,
    git: bool,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let who = if git { "fetchGit" } else { "fetchTree" };
    let mut attrs = BTreeMap::new();
    let value = l.force(m, r, arg, pos)?;
    if let ValueView::Attrs(set) = m.heap.view(value) {
        let set = set.root(r);
        let mut ty = if git { Some("git".to_owned()) } else { None };
        if let Some(attr) = m.heap.bindings_view(set).get(sym::TYPE).map(|v| v.root(r)) {
            if ty.is_some() {
                return Err(ev.error_at(ErrorKind::Eval, "unexpected argument 'type'", pos));
            }
            let text = force_string_no_ctx(
                l,
                m,
                r,
                attr.value,
                attr.pos,
                &format!("while evaluating the `type` argument passed to '{who}'"),
            )?;
            ty = Some(
                fetch_text(
                    ev,
                    &text,
                    || msg!("argument 'type' to '", who, "'"),
                    attr.pos,
                )?
                .to_owned(),
            );
        } else if ty.is_none() {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!("argument 'type' is missing in call to '{who}'"),
                pos,
            ));
        }
        attrs.insert("type".into(), FetchAttr::String(ty.unwrap()));
        for attr in bindings::attributes(&m.heap, r, set) {
            if attr.name == sym::TYPE {
                continue;
            }
            let name = ev.symbol_name(attr.name);
            let name = fetch_text(
                ev,
                name,
                || msg!("the name of argument '", name, "' to '", who, "'"),
                attr.pos,
            )?
            .to_owned();
            let value = m.with_roots(|m, t| {
                let value = l.force(m, t, attr.value, attr.pos)?;
                match m.heap.view(value) {
                    ValueView::Path(_) | ValueView::String(_) => {
                        let bytes = coerced(l, m, t, attr.value, attr.pos, "")?;
                        if name == "narHash" { check_nar_hash_bytes(ev, &bytes)?; }
                        Ok(FetchAttr::String(fetch_text(ev, &bytes, || msg!("argument '", name, "' to '", who, "'"), attr.pos)?.to_owned()))
                    }
                    ValueView::Bool(b) => Ok(FetchAttr::Bool(b)),
                    ValueView::Int(n) => {
                        if n < 0 { return Err(ev.error_at(ErrorKind::Eval, format!("negative value given for '{who}' argument '{name}': {n}"), pos)); }
                        Ok(FetchAttr::Int(n as u64))
                    }
                    _ if name == "publicKeys" => {
                        ev.require_experimental("verified-fetches", "")?;
                        let json = strings::call(l, m, t, "toJSON", &[attr.value], pos).expect("JSON builtin")?;
                        let json = strings::materialize(l, m, t, json)?;
                        let bytes = strings::snapshot(m, json).0;
                        Ok(FetchAttr::String(String::from_utf8(bytes).expect("JSON is UTF-8")))
                    }
                    _ => Err(ev.error(ErrorKind::TypeError, format!("argument '{name}' to '{who}' is {} while a string, Boolean or integer is expected", crate::managed_print::describe_type(l, m, value)))),
                }
            })?;
            attrs.insert(name, value);
        }
        if git
            && !attrs.contains_key("exportIgnore")
            && !matches!(attrs.get("submodules"), Some(FetchAttr::Bool(true)))
        {
            attrs.insert("exportIgnore".into(), FetchAttr::Bool(true));
        }
        if !git && attrs.contains_key("name") {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!("argument 'name' isn’t supported in call to '{who}'"),
                pos,
            ));
        }
    } else {
        let bytes = coerced(
            l,
            m,
            r,
            arg,
            pos,
            &format!("while evaluating the first argument passed to '{who}'"),
        )?;
        let url = fetch_text(ev, &bytes, || msg!("the URL passed to '", who, "'"), pos)?.to_owned();
        if git {
            attrs.insert("type".into(), FetchAttr::String("git".into()));
            attrs.insert("exportIgnore".into(), FetchAttr::Bool(true));
        }
        attrs.insert("url".into(), FetchAttr::String(url));
    }
    let tree = m
        .heap
        .blocking(|| {
            ev.host.fetch_tree(&FetchRequest {
                attrs,
                who,
                empty_rev_fallback: git,
                nar_hash_is_final: true,
                refetch: false,
            })
        })
        .map_err(|e| fetch_failed(ev, e))?;
    let path = mount_tree(ev, &tree)?;
    emit(l, m, r, &path, &tree.attrs)
}

#[allow(clippy::too_many_arguments)]
fn fetch<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    arg: CellRoot<'_, 'h>,
    pos: SrcPos,
    who: &'static str,
    unpack: bool,
    default_name: &str,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let mut url = None;
    let mut expected_hash = None;
    let mut name = default_name.to_owned();
    let value = l.force(m, r, arg, pos)?;
    let is_attrs = matches!(m.heap.view(value), ValueView::Attrs(_));
    let mut name_passed = false;
    if let ValueView::Attrs(set) = m.heap.view(value) {
        let set = set.root(r);
        for attr in bindings::attributes(&m.heap, r, set) {
            let key = ev.symbol_name(attr.name);
            match key {
                b"url" => {
                    let bytes = force_string_no_ctx(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the url we should fetch",
                    )?;
                    url = Some(
                        fetch_text(
                            ev,
                            &bytes,
                            || msg!("argument 'url' to '", who, "'"),
                            attr.pos,
                        )?
                        .to_owned(),
                    );
                }
                b"sha256" => {
                    let bytes = force_string_no_ctx(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the sha256 of the content we should fetch",
                    )?;
                    expected_hash = Some(new_hash_allow_empty(ev, &bytes, Some(HashAlgo::Sha256))?);
                }
                b"name" => {
                    name_passed = true;
                    let bytes = force_string_no_ctx(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the name of the content we should fetch",
                    )?;
                    name = fetch_text(
                        ev,
                        &bytes,
                        || msg!("argument 'name' to '", who, "'"),
                        attr.pos,
                    )?
                    .to_owned();
                }
                _ => {
                    return Err(ev.error_at(
                        ErrorKind::Eval,
                        msg!("unsupported argument '", key, "' to '", who, "'"),
                        pos,
                    ))
                }
            }
        }
        if url.is_none() {
            return Err(ev.error_at(ErrorKind::Eval, "'url' argument required", pos));
        }
    } else {
        let bytes = force_string_no_ctx(
            l,
            m,
            r,
            arg,
            pos,
            "while evaluating the url we should fetch",
        )?;
        url =
            Some(fetch_text(ev, &bytes, || msg!("the URL passed to '", who, "'"), pos)?.to_owned());
    }
    let url = url.unwrap();
    if name.is_empty() {
        name = url.rsplit('/').next().unwrap_or("").to_owned();
    }
    if let Err(e) = brix_store::store_path::validate_name(&name) {
        let resolution = if name_passed {
            format!("Please change the value for the 'name' attribute passed to '{who}', so that it can create a valid store path.")
        } else if is_attrs {
            format!("Please add a valid 'name' attribute to the argument for '{who}', so that it can create a valid store path.")
        } else {
            format!("Please pass an attribute set with 'url' and 'name' attributes to '{who}',  so that it can create a valid store path.")
        };
        return Err(ev.error_at(
            ErrorKind::Eval,
            msg!(
                format!("invalid store path name when fetching URL '{url}': "),
                e.message_bytes(),
                ". ",
                resolution
            ),
            pos,
        ));
    }
    let Some(h) = expected_hash else {
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!("in pure evaluation mode, '{who}' requires a 'sha256' argument"),
            pos,
        ));
    };
    let expected_path = if h.algo() == HashAlgo::Sha256 {
        Some(
            ev.config
                .store_dir
                .make_fixed_output_path(
                    &name,
                    &FixedOutputInfo {
                        method: if unpack {
                            FileIngestionMethod::NixArchive
                        } else {
                            FileIngestionMethod::Flat
                        },
                        hash: h,
                        references: Default::default(),
                    },
                )
                .map_err(|e| ev.error(ErrorKind::Eval, e.to_string()))?,
        )
    } else {
        None
    };
    // Pinned and already there (`ensurePath`: valid in the store or
    // substitutable): brix's own store, then the host's.
    if let Some(ep) = &expected_path {
        let in_store = ev.store.borrow().contains(ep);
        if in_store
            || ev
                .host
                .is_valid_path(ep)
                .map_err(|e| ev.error_at(ErrorKind::Eval, e, pos))?
        {
            return store_path_string(l, m, r, ep);
        }
    }
    let fetch_error = |msg: String| ev.error_at(ErrorKind::Unsupported, msg, pos);
    if unpack {
        let mut attrs = BTreeMap::new();
        attrs.insert("type".into(), FetchAttr::String("tarball".into()));
        attrs.insert("url".into(), FetchAttr::String(url.clone()));
        attrs.insert("name".into(), FetchAttr::String(name.clone()));
        attrs.insert(
            "narHash".into(),
            FetchAttr::String(h.to_string(HashFormat::Sri, true)),
        );
        let tree = m
            .heap
            .blocking(|| {
                ev.host.fetch_tree(&FetchRequest {
                    attrs,
                    who,
                    empty_rev_fallback: false,
                    nar_hash_is_final: false,
                    refetch: false,
                })
            })
            .map_err(|e| fetch_failed(ev, e))?;
        let sp = mount_tree(ev, &tree)?;
        store_path_string(l, m, r, &sp)
    } else {
        let file = m
            .heap
            .blocking(|| ev.host.fetch_file(&url, &name))
            .map_err(fetch_error)?;
        let actual = Hash::of(HashAlgo::Sha256, &file.contents);
        if actual != h {
            let e = ev.error(
                ErrorKind::Eval,
                format!(
                    "hash mismatch in file downloaded from '{url}':\n  specified: {}\n  got:       {}",
                    h.to_string(HashFormat::Nix32, true),
                    actual.to_string(HashFormat::Nix32, true)
                ),
            );
            e.exit_status.set(102);
            return Err(e);
        }
        // `downloadFile` adds the file to the store, so the path is valid
        // (`readFile`, a derivation's input) and its contents are brix's
        // to provide.
        let at = ev.store_path_mount_point(&file.store_path);
        let flat = PathIngestion {
            method: ContentAddressMethod::Flat,
            algo: HashAlgo::Sha256,
            references: Default::default(),
        };
        let sp = ev
            .store
            .borrow_mut()
            .add_hashed_path(
                &SourcePath::new(ev.root_accessor, at),
                &name,
                &flat,
                actual,
                false,
            )
            .map_err(|e| ev.error(ErrorKind::Eval, e.to_string()))?;
        if sp != file.store_path {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!(
                    "the fetcher stored '{url}' at '{}', but its contents belong at '{}'",
                    ev.config.store_dir.display(&file.store_path),
                    ev.config.store_dir.display(&sp)
                ),
                pos,
            ));
        }
        ev.mount_store_file(&sp, file.contents);
        store_path_string(l, m, r, &sp)
    }
}

fn mercurial<'r, 'h>(
    l: &Language<'_>,
    m: &mut Machine<'h>,
    r: &Roots<'r, 'h>,
    arg: CellRoot<'_, 'h>,
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = l.legacy();
    let mut url = String::new();
    let mut revision = None;
    let mut name = "source".to_owned();
    let value = l.force(m, r, arg, pos)?;
    if let ValueView::Attrs(set) = m.heap.view(value) {
        let set = set.root(r);
        for attr in bindings::attributes(&m.heap, r, set) {
            match ev.symbol_name(attr.name) {
                b"url" => {
                    let bytes = coerced(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the `url` attribute passed to builtins.fetchMercurial",
                    )?;
                    url = fetch_text(
                        ev,
                        &bytes,
                        || msg!("argument 'url' to 'fetchMercurial'"),
                        attr.pos,
                    )?
                    .to_owned();
                }
                b"rev" => {
                    let bytes = force_string_no_ctx(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the `rev` attribute passed to builtins.fetchMercurial",
                    )?;
                    revision = (bytes.len() == 40 && bytes.iter().all(u8::is_ascii_hexdigit))
                        .then(|| String::from_utf8_lossy(&bytes).to_ascii_lowercase());
                }
                b"name" => {
                    let bytes = force_string_no_ctx(
                        l,
                        m,
                        r,
                        attr.value,
                        attr.pos,
                        "while evaluating the `name` attribute passed to builtins.fetchMercurial",
                    )?;
                    name = fetch_text(
                        ev,
                        &bytes,
                        || msg!("argument 'name' to 'fetchMercurial'"),
                        attr.pos,
                    )?
                    .to_owned();
                }
                key => {
                    return Err(ev.error_at(
                        ErrorKind::Eval,
                        msg!("unsupported argument '", key, "' to 'fetchMercurial'"),
                        attr.pos,
                    ))
                }
            }
        }
        if url.is_empty() {
            return Err(ev.error_at(ErrorKind::Eval, "'url' argument required", pos));
        }
    } else {
        let bytes = coerced(
            l,
            m,
            r,
            arg,
            pos,
            "while evaluating the first argument passed to builtins.fetchMercurial",
        )?;
        url = fetch_text(
            ev,
            &bytes,
            || msg!("the URL passed to 'fetchMercurial'"),
            pos,
        )?
        .to_owned();
    }
    let Some(revision) = revision else {
        return Err(ev.error(
            ErrorKind::Eval,
            "in pure evaluation mode, 'fetchMercurial' requires a Mercurial revision",
        ));
    };
    if !url.contains("://") {
        url = format!("file://{url}");
    }
    let tree = m
        .heap
        .blocking(|| {
            ev.host.fetch_tree(&FetchRequest {
                attrs: BTreeMap::from([
                    ("type".into(), FetchAttr::String("hg".into())),
                    ("url".into(), FetchAttr::String(url)),
                    ("name".into(), FetchAttr::String(name)),
                    ("rev".into(), FetchAttr::String(revision)),
                ]),
                who: "fetchMercurial",
                empty_rev_fallback: false,
                nar_hash_is_final: false,
                refetch: true,
            })
        })
        .map_err(|e| fetch_failed(ev, e))?;
    let path = mount_tree(ev, &tree)?;
    let mut attrs = BTreeMap::new();
    if let Some(FetchAttr::String(branch)) = tree.locked_attrs.get("ref") {
        attrs.insert("branch".into(), FetchAttr::String(branch.clone()));
    }
    let revision = match tree.locked_attrs.get("rev") {
        Some(FetchAttr::String(revision)) => revision.clone(),
        _ => "0".repeat(40),
    };
    attrs.insert(
        "shortRev".into(),
        FetchAttr::String(revision.get(..12).unwrap_or(&revision).to_owned()),
    );
    attrs.insert("rev".into(), FetchAttr::String(revision));
    if let Some(FetchAttr::Int(count)) = tree.locked_attrs.get("revCount") {
        attrs.insert("revCount".into(), FetchAttr::Int(*count));
    }
    emit(l, m, r, &path, &attrs)
}

#[cfg(all(test, not(brix_parallel)))]
mod tests {
    use super::*;
    use crate::host::{FetchError, FetchedFile, FetchedTree};
    use crate::managed_heap::with_owned_heap;
    use std::cell::RefCell;
    use std::sync::Arc;

    #[derive(Default)]
    struct FetchHost {
        requests: RefCell<Vec<FetchRequest>>,
    }
    impl crate::Host for FetchHost {
        fn root_accessor(&self) -> Arc<dyn brix_store::SourceAccessor> {
            Arc::new(brix_store::MemoryAccessor::new())
        }
        fn fetch_tree(
            &self,
            request: &FetchRequest,
        ) -> std::result::Result<FetchedTree, FetchError> {
            self.requests.borrow_mut().push(request.clone());
            if matches!(request.attrs.get("url"), Some(FetchAttr::String(url)) if url == "fail") {
                return Err(FetchError {
                    message: "test fetch failed".into(),
                    traces: vec!["while fetching test input".into()],
                    exit_status: 102,
                    foreign: false,
                });
            }
            let hash = Hash::of(HashAlgo::Sha256, b"test NAR");
            let name = match request.attrs.get("name") {
                Some(FetchAttr::String(name)) => name.as_str(),
                _ => "source",
            };
            let store_path = brix_store::StoreDir::default()
                .make_fixed_output_path(
                    name,
                    &FixedOutputInfo {
                        method: FileIngestionMethod::NixArchive,
                        hash,
                        references: Default::default(),
                    },
                )
                .unwrap();
            let mut accessor = brix_store::MemoryAccessor::new();
            accessor
                .add_file(
                    &brix_store::CanonPath::new("/file").unwrap(),
                    b"fetched",
                    false,
                )
                .unwrap();
            Ok(FetchedTree {
                accessor: Arc::new(accessor),
                store_path,
                attrs: BTreeMap::from([
                    (
                        "narHash".into(),
                        FetchAttr::String(hash.to_string(HashFormat::Sri, true)),
                    ),
                    ("revCount".into(), FetchAttr::Int(7)),
                    ("rev".into(), FetchAttr::String("a".repeat(40))),
                ]),
                locked_attrs: BTreeMap::from([
                    ("ref".into(), FetchAttr::String("main".into())),
                    ("rev".into(), FetchAttr::String("b".repeat(40))),
                    ("revCount".into(), FetchAttr::Int(9)),
                ]),
                locked_url: "test:locked".into(),
                display_prefix: None,
            })
        }
        fn fetch_file(&self, _url: &str, name: &str) -> std::result::Result<FetchedFile, String> {
            let contents = b"downloaded".to_vec();
            let hash = Hash::of(HashAlgo::Sha256, &contents);
            let store_path = brix_store::StoreDir::default()
                .make_fixed_output_path(
                    name,
                    &FixedOutputInfo {
                        method: FileIngestionMethod::Flat,
                        hash,
                        references: Default::default(),
                    },
                )
                .unwrap();
            Ok(FetchedFile {
                contents,
                store_path,
            })
        }
    }

    #[test]
    fn managed_fetches_shape_host_requests_and_keep_results_alive_through_moves() {
        for stress in [false, true] {
            let arena = crate::Arena::new();
            let host = FetchHost::default();
            let language = Language::new(crate::Evaluator::new(
                &arena,
                crate::Config::default(),
                &host,
            ));
            with_owned_heap(brix_gc::Config { workers: 0, chunk_bytes: 256, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: 1024, stress }, |heap| {
                let mut machine = Machine::new(heap);
                let hash = Hash::of(HashAlgo::Sha256, b"downloaded").to_string(HashFormat::Sri, true);
                let sources = [
                    "let t = builtins.fetchTree { type = \"git\"; url = \"test\"; revCount = 0; submodules = true; }; in t.revCount == 7 && builtins.hasContext t.outPath && builtins.readFile (t.outPath + \"/file\") == \"fetched\"".to_owned(),
                    "(builtins.fetchGit \"test\").revCount == 7".to_owned(),
                    "(builtins.fetchGit { url = \"test\"; submodules = true; }).revCount == 7".to_owned(),
                    "let t = builtins.fetchMercurial { url = \"/repo\"; rev = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"; }; in t.branch == \"main\" && t.rev == \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\" && t.shortRev == \"bbbbbbbbbbbb\" && t.revCount == 9".to_owned(),
                    format!("builtins.readFile (builtins.fetchurl {{ url = \"https://test/file\"; sha256 = \"{hash}\"; }}) == \"downloaded\""),
                    format!("builtins.readFile ((builtins.fetchTarball {{ url = \"https://test/tree\"; sha256 = \"{hash}\"; name = \"tarball\"; }}) + \"/file\") == \"fetched\""),
                ];
                for source in &sources {
                    machine.with_roots(|machine, roots| {
                        let value = language.eval_str(machine, roots, source.as_bytes(), "/").unwrap_or_else(|error| panic!("{source}: {}", error.render(true)));
                        assert!(matches!(machine.heap.view(value), ValueView::Bool(true)), "{source}");
                        machine.heap.collect().unwrap();
                        assert!(matches!(machine.heap.view(value), ValueView::Bool(true)));
                    });
                }
            }).unwrap();
            let requests = host.requests.borrow();
            let git: Vec<_> = requests.iter().filter(|r| r.who == "fetchGit").collect();
            assert_eq!(git.len(), 2);
            assert_eq!(
                git[0].attrs.get("exportIgnore"),
                Some(&FetchAttr::Bool(true))
            );
            assert!(!git[1].attrs.contains_key("exportIgnore"));
            assert!(git
                .iter()
                .all(|r| r.empty_rev_fallback && r.nar_hash_is_final && !r.refetch));
            let hg = requests.iter().find(|r| r.who == "fetchMercurial").unwrap();
            assert!(hg.refetch && !hg.nar_hash_is_final);
            assert_eq!(
                hg.attrs.get("url"),
                Some(&FetchAttr::String("file:///repo".into()))
            );
            assert_eq!(
                hg.attrs.get("rev"),
                Some(&FetchAttr::String("a".repeat(40)))
            );
            let tar = requests.iter().find(|r| r.who == "fetchTarball").unwrap();
            assert!(!tar.nar_hash_is_final);
        }
    }

    #[test]
    fn managed_fetch_errors_preserve_checks_traces_and_exit_status() {
        let arena = crate::Arena::new();
        let host = FetchHost::default();
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
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
                for (source, expected) in [
                    (
                        "builtins.fetchTree {}",
                        "argument 'type' is missing in call to 'fetchTree'",
                    ),
                    (
                        "builtins.fetchTree { type = \"git\"; name = \"bad\"; }",
                        "argument 'name' isn’t supported in call to 'fetchTree'",
                    ),
                    (
                        "builtins.fetchGit { type = \"git\"; }",
                        "unexpected argument 'type'",
                    ),
                    (
                        "builtins.fetchGit { url = \"test\"; revCount = -1; }",
                        "negative value given for 'fetchGit' argument 'revCount': -1",
                    ),
                    (
                        "builtins.fetchurl \"https://test/file\"",
                        "in pure evaluation mode, 'fetchurl' requires a 'sha256' argument",
                    ),
                    (
                        "builtins.fetchMercurial { url = \"/repo\"; rev = \"main\"; }",
                        "in pure evaluation mode, 'fetchMercurial' requires a Mercurial revision",
                    ),
                ] {
                    machine.with_roots(|machine, roots| {
                        let error = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .expect_err(source);
                        assert!(
                            error
                                .render(true)
                                .windows(expected.len())
                                .any(|w| w == expected.as_bytes()),
                            "{source}: {}",
                            error.render(true)
                        );
                    });
                }
                machine.with_roots(|machine, roots| {
                    let error = language
                        .eval_str(machine, roots, b"builtins.fetchGit \"fail\"", "/")
                        .expect_err("fetch failure");
                    assert_eq!(error.exit_status.get(), 102);
                    assert!(error
                        .render(true)
                        .windows(25)
                        .any(|w| w == b"while fetching test input"));
                });
            },
        )
        .unwrap();
    }
}
