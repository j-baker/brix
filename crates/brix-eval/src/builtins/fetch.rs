//! `fetchTree`, `fetchGit`, `fetchTarball`, `fetchurl`, `fetchMercurial`
//! (`primops/fetchTree.cc`). The fetching itself is the host's job; this
//! module does Nix's argument handling, pure-mode checks and the shaping
//! of the result.

use std::collections::BTreeMap;
use std::sync::Arc;

use brix_store::{
    ContentAddressMethod, FileIngestionMethod, FixedOutputInfo, Hash, HashAlgo, HashFormat,
    PathIngestion, SourceAccessor, SourcePath, StorePath,
};

use crate::builtins::paths::new_hash_allow_empty;
use crate::builtins::reg;
use crate::builtins::strings::xp_settings;
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::host::{FetchAttr, FetchError, FetchRequest, FetchedTree};
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{Attr, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "fetchTree", 1, prim_fetch_tree);
    reg(ev, "__fetchurl", 1, prim_fetchurl);
    reg(ev, "fetchTarball", 1, prim_fetch_tarball);
    reg(ev, "fetchGit", 1, prim_fetch_git);
    reg(ev, "fetchMercurial", 1, prim_fetch_mercurial);
    if ev.experimental("fetch-closure") {
        reg(ev, "__fetchClosure", 1, prim_fetch_closure);
    }
}

struct FetchTreeParams {
    empty_rev_fallback: bool,
    allow_name_argument: bool,
    is_fetch_git: bool,
}

/// Mounts a fetched tree at its store path and returns that path.
pub(crate) fn mount_tree<'a>(ev: &Evaluator<'a>, tree: &FetchedTree) -> Result<StorePath> {
    let sp = tree.store_path.clone();
    let at = ev.store_path_mount_point(&sp);
    // Paths inside the tree are shown as `«<locked input>»/…`.
    // A host with a compiled image of the tree serves the files from it
    // and lets imports link precompiled code.
    let image = ev
        .host
        .compiled_image(tree, ev.image_compile_context())
        .map_err(|e| ev.error(ErrorKind::Eval, e))?;
    let inner: Arc<dyn SourceAccessor> = match &image {
        // Files the image does not store (everything but `.nix` files,
        // unless it was compiled with the tree) come from the tree itself.
        Some(img) => Arc::new(
            crate::image::ImageAccessor::new(img.clone()).with_fallback(tree.accessor.clone()),
        ),
        None => tree.accessor.clone(),
    };
    let prefix = match &tree.display_prefix {
        Some(p) => p.clone(),
        None => format!("«{}»", tree.locked_url),
    };
    let shown = Arc::new(crate::mount::DisplayPrefixAccessor::new(inner, prefix));
    ev.mount(at.clone(), shown);
    if let Some(img) = image {
        ev.register_image(at.clone(), img)
            .map_err(|e| ev.error(ErrorKind::Eval, e.to_string()))?;
    }
    // The tree's store object, so that copying it again (`"${tree}"`)
    // finds it.
    if let Some(FetchAttr::String(h)) = tree.attrs.get("narHash") {
        let h = Hash::parse_any(h, None, xp_settings(ev))
            .map_err(|e| ev.error(crate::builtins::strings::hash_error_kind(&e), e.to_string()))?;
        ev.store
            .borrow_mut()
            .add_known_tree(
                &brix_store::SourcePath::new(ev.root_accessor, at.clone()),
                sp.name(),
                h,
            )
            .map_err(|e| ev.error(ErrorKind::Eval, e.to_string()))?;
    }
    ev.allow_path(at);
    Ok(sp)
}

/// A fetcher argument as text: `what` names it in the error. Nix keeps the
/// bytes, but the fetchers (URLs, refs, revisions, names, paths) take
/// text in brix, so an argument that is not UTF-8 is refused rather than
/// altered (`KNOWN_DEVIATIONS.md`).
pub(crate) fn fetch_text<'s>(
    ev: &Evaluator<'_>,
    s: &'s [u8],
    what: impl FnOnce() -> Vec<u8>,
    pos: SrcPos,
) -> Result<&'s str> {
    std::str::from_utf8(s).map_err(|_| {
        ev.error_at(
            ErrorKind::Eval,
            msg!(
                what(),
                " is not valid UTF-8; brix does not support non-UTF-8 fetcher arguments"
            ),
            pos,
        )
    })
}

/// The evaluation error of a failed fetch: Nix's error as the fetchers
/// raised it (a plain `Error`, without a position), with their traces
/// ("while fetching the input '…'") below the ones evaluation adds as it
/// propagates, and their exit status.
pub(crate) fn fetch_failed(ev: &Evaluator<'_>, e: FetchError) -> crate::Error {
    if e.foreign {
        return ev.error(ErrorKind::Foreign, e.message);
    }
    let err = ev.error(ErrorKind::Unsupported, e.message);
    // The evaluator keeps traces innermost first.
    for trace in e.traces.into_iter().rev() {
        err.add_trace(None, trace);
    }
    err.exit_status.set(e.exit_status);
    err
}

/// `Input::getNarHash` of a `narHash` argument that is not UTF-8: no SRI
/// hash is, so Nix's `Hash::parseSRI` fails, with a message quoting the
/// bytes, which is the error raised here. A UTF-8 `narHash` is the
/// fetcher's to parse.
pub(crate) fn check_nar_hash_bytes(ev: &Evaluator<'_>, s: &[u8]) -> Result<()> {
    if std::str::from_utf8(s).is_err() {
        if let Err(e) = Hash::parse_sri(s, xp_settings(ev)) {
            return Err(crate::builtins::strings::hash_error(ev, &e));
        }
    }
    Ok(())
}

/// `emitTreeAttrs`: `outPath`, with its store-path context, and the
/// attributes the host computed for the locked input.
fn emit_tree_attrs<'a>(ev: &Evaluator<'a>, sp: &StorePath, tree: &FetchedTree) -> Value<'a> {
    let mut attrs: Vec<Attr<'a>> = Vec::with_capacity(tree.attrs.len() + 1);
    attrs.push(ev.attr(sym::OUT_PATH, ev.store_path_string(sp)));
    for (name, value) in &tree.attrs {
        let value = match value {
            FetchAttr::String(s) => ev.mk_string(s),
            // `mkInt` of Nix's `uint64_t`.
            FetchAttr::Int(n) => Value::Int(*n as i64),
            FetchAttr::Bool(b) => Value::Bool(*b),
        };
        attrs.push(ev.attr(ev.sym(name), value));
    }
    ev.mk_attrs(attrs, SrcPos::NONE)
}

fn fetch_tree<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    params: FetchTreeParams,
) -> Result<Value<'a>> {
    let fetcher: &'static str = if params.is_fetch_git {
        "fetchGit"
    } else {
        "fetchTree"
    };
    let mut attrs: BTreeMap<String, FetchAttr> = BTreeMap::new();
    let mut context = StringContext::new();
    let v = ev.force(args[0], pos)?;
    if let Value::Attrs(b) = v {
        let mut ty: Option<String> = if params.is_fetch_git {
            Some("git".into())
        } else {
            None
        };
        if let Some(a) = b.get(sym::TYPE) {
            if ty.is_some() {
                return Err(ev.error_at(ErrorKind::Eval, "unexpected argument 'type'", pos));
            }
            let s = ev.force_string_no_ctx(
                a.value,
                a.pos,
                &format!("while evaluating the `type` argument passed to '{fetcher}'"),
            )?;
            let what = || msg!("argument 'type' to '", fetcher, "'");
            ty = Some(fetch_text(ev, s, what, a.pos)?.to_owned());
        } else if ty.is_none() {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!("argument 'type' is missing in call to '{fetcher}'"),
                pos,
            ));
        }
        attrs.insert("type".into(), FetchAttr::String(ty.unwrap()));
        for a in b.iter() {
            if a.name == sym::TYPE {
                continue;
            }
            let name = ev.symbol_name(a.name);
            let what = || msg!("the name of argument '", name, "' to '", fetcher, "'");
            let name = fetch_text(ev, name, what, a.pos)?.to_owned();
            let av = ev.force(a.value, a.pos)?;
            match av {
                Value::Path(_) | Value::String(_) => {
                    let s = ev.coerce_to_string(a.pos, a.value, &mut context, "", Coercion::NO_COPY)?;
                    if name == "narHash" {
                        check_nar_hash_bytes(ev, s)?;
                    }
                    let what = || msg!("argument '", &name, "' to '", fetcher, "'");
                    let s = fetch_text(ev, s, what, a.pos)?.to_owned();
                    attrs.insert(name, FetchAttr::String(s));
                }
                Value::Bool(bv) => {
                    attrs.insert(name, FetchAttr::Bool(bv));
                }
                Value::Int(i) => {
                    if i < 0 {
                        return Err(ev.error_at(
                            ErrorKind::Eval,
                            format!("negative value given for '{fetcher}' argument '{name}': {i}"),
                            pos,
                        ));
                    }
                    attrs.insert(name, FetchAttr::Int(i as u64));
                }
                // A Git input's keys may be given as a list: the input
                // gets their JSON (`printValueAsJSON`, strict, copying
                // paths to the store).
                _ if name == "publicKeys" => {
                    ev.require_experimental("verified-fetches", "")?;
                    let json = ev.to_json(a.value, pos, &mut context, true, true)?;
                    attrs.insert(name, FetchAttr::String(json));
                }
                _ => {
                    return Err(ev.error(
                        ErrorKind::TypeError,
                        format!(
                            "argument '{name}' to '{fetcher}' is {} while a string, Boolean or integer is expected",
                            av.show_type()
                        ),
                    ))
                }
            }
        }
        if params.is_fetch_git
            && !attrs.contains_key("exportIgnore")
            && !matches!(attrs.get("submodules"), Some(FetchAttr::Bool(true)))
        {
            attrs.insert("exportIgnore".into(), FetchAttr::Bool(true));
        }
        if !params.allow_name_argument && attrs.contains_key("name") {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!("argument 'name' isn’t supported in call to '{fetcher}'"),
                pos,
            ));
        }
    } else {
        let url = ev.coerce_to_string(
            pos,
            args[0],
            &mut context,
            &format!("while evaluating the first argument passed to '{fetcher}'"),
            Coercion::NO_COPY,
        )?;
        let what = || msg!("the URL passed to '", fetcher, "'");
        let url = fetch_text(ev, url, what, pos)?.to_owned();
        if params.is_fetch_git {
            attrs.insert("type".into(), FetchAttr::String("git".into()));
            attrs.insert("url".into(), FetchAttr::String(url));
            attrs.insert("exportIgnore".into(), FetchAttr::Bool(true));
        } else {
            attrs.insert("url".into(), FetchAttr::String(url));
        }
    }
    let request = FetchRequest {
        attrs,
        who: fetcher,
        empty_rev_fallback: params.empty_rev_fallback,
        nar_hash_is_final: true,
        refetch: false,
    };
    let tree = ev
        .host
        .fetch_tree(&request)
        .map_err(|e| fetch_failed(ev, e))?;
    let sp = mount_tree(ev, &tree)?;
    Ok(emit_tree_attrs(ev, &sp, &tree))
}

fn prim_fetch_tree<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    fetch_tree(
        ev,
        pos,
        args,
        FetchTreeParams {
            empty_rev_fallback: false,
            allow_name_argument: false,
            is_fetch_git: false,
        },
    )
}

fn prim_fetch_git<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    fetch_tree(
        ev,
        pos,
        args,
        FetchTreeParams {
            empty_rev_fallback: true,
            allow_name_argument: true,
            is_fetch_git: true,
        },
    )
}

/// `fetch`: `fetchurl` and `fetchTarball`.
fn fetch<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    who: &'static str,
    unpack: bool,
    default_name: &str,
) -> Result<Value<'a>> {
    let mut url: Option<String> = None;
    let mut expected_hash: Option<Hash> = None;
    let mut name = default_name.to_owned();
    let v = ev.force(args[0], pos)?;
    let is_attrs = matches!(v, Value::Attrs(_));
    let mut name_passed = false;
    if let Value::Attrs(b) = v {
        for a in b.iter() {
            let n = ev.symbol_name(a.name);
            match n {
                b"url" => {
                    let s = ev.force_string_no_ctx(
                        a.value,
                        a.pos,
                        "while evaluating the url we should fetch",
                    )?;
                    let what = || msg!("argument 'url' to '", who, "'");
                    url = Some(fetch_text(ev, s, what, a.pos)?.to_owned());
                }
                b"sha256" => {
                    let s = ev.force_string_no_ctx(
                        a.value,
                        a.pos,
                        "while evaluating the sha256 of the content we should fetch",
                    )?;
                    // Bytes, quoted as they are when not a hash.
                    expected_hash = Some(new_hash_allow_empty(ev, s, Some(HashAlgo::Sha256))?);
                }
                b"name" => {
                    name_passed = true;
                    let s = ev.force_string_no_ctx(
                        a.value,
                        a.pos,
                        "while evaluating the name of the content we should fetch",
                    )?;
                    let what = || msg!("argument 'name' to '", who, "'");
                    name = fetch_text(ev, s, what, a.pos)?.to_owned();
                }
                _ => {
                    return Err(ev.error_at(
                        ErrorKind::Eval,
                        msg!("unsupported argument '", n, "' to '", who, "'"),
                        pos,
                    ))
                }
            }
        }
        if url.is_none() {
            return Err(ev.error_at(ErrorKind::Eval, "'url' argument required", pos));
        }
    } else {
        let s = ev.force_string_no_ctx(args[0], pos, "while evaluating the url we should fetch")?;
        let what = || msg!("the URL passed to '", who, "'");
        url = Some(fetch_text(ev, s, what, pos)?.to_owned());
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
            return Ok(ev.store_path_string(ep));
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
        let tree = ev
            .host
            .fetch_tree(&FetchRequest {
                attrs,
                who,
                empty_rev_fallback: false,
                nar_hash_is_final: false,
                refetch: false,
            })
            .map_err(|e| fetch_failed(ev, e))?;
        let sp = mount_tree(ev, &tree)?;
        Ok(ev.store_path_string(&sp))
    } else {
        let file = ev.host.fetch_file(&url, &name).map_err(fetch_error)?;
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
        Ok(ev.store_path_string(&sp))
    }
}

fn prim_fetchurl<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    fetch(ev, pos, args, "fetchurl", false, "")
}

fn prim_fetch_tarball<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    fetch(ev, pos, args, "fetchTarball", true, "source")
}

/// `fetchMercurial` (`primops/fetchMercurial.cc`): a URL, or `url`,
/// `rev` (a revision, or else a branch or tag name) and `name`. It
/// fetches the `hg` input with `Input::fetchToStore` and returns
/// `outPath` and, from the locked input rather than `emitTreeAttrs`,
/// `branch` (its `ref`), `rev` and a `shortRev` of 12 digits (all zeros
/// for a working tree with uncommitted changes) and `revCount`.
fn prim_fetch_mercurial<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut url = String::new();
    let mut rev: Option<String> = None;
    let mut name = "source".to_owned();
    let mut context = StringContext::new();
    let v = ev.force(args[0], pos)?;
    if let Value::Attrs(b) = v {
        for a in b.iter() {
            let n = ev.symbol_name(a.name);
            match n {
                b"url" => {
                    let s = ev.coerce_to_string(
                        a.pos,
                        a.value,
                        &mut context,
                        "while evaluating the `url` attribute passed to builtins.fetchMercurial",
                        Coercion::NO_COPY,
                    )?;
                    let what = || msg!("argument 'url' to 'fetchMercurial'");
                    url = fetch_text(ev, s, what, a.pos)?.to_owned();
                }
                b"rev" => {
                    let s = ev.force_string_no_ctx(
                        a.value,
                        a.pos,
                        "while evaluating the `rev` attribute passed to builtins.fetchMercurial",
                    )?;
                    // Unlike `fetchGit`'s, this `rev` is a revision when it
                    // matches `revRegex` (40 hex digits) and the input's
                    // `ref` otherwise, which pure evaluation, brix's only
                    // mode, never gets to fetch (below).
                    rev = (s.len() == 40 && s.iter().all(u8::is_ascii_hexdigit))
                        .then(|| String::from_utf8_lossy(s).to_ascii_lowercase());
                }
                b"name" => {
                    let s = ev.force_string_no_ctx(
                        a.value,
                        a.pos,
                        "while evaluating the `name` attribute passed to builtins.fetchMercurial",
                    )?;
                    let what = || msg!("argument 'name' to 'fetchMercurial'");
                    name = fetch_text(ev, s, what, a.pos)?.to_owned();
                }
                _ => {
                    return Err(ev.error_at(
                        ErrorKind::Eval,
                        msg!("unsupported argument '", n, "' to 'fetchMercurial'"),
                        a.pos,
                    ))
                }
            }
        }
        if url.is_empty() {
            return Err(ev.error_at(ErrorKind::Eval, "'url' argument required", pos));
        }
    } else {
        let s = ev.coerce_to_string(
            pos,
            args[0],
            &mut context,
            "while evaluating the first argument passed to builtins.fetchMercurial",
            Coercion::NO_COPY,
        )?;
        let what = || msg!("the URL passed to 'fetchMercurial'");
        url = fetch_text(ev, s, what, pos)?.to_owned();
    }
    // A plain `Error`, without a position of its own.
    let Some(rev) = rev else {
        return Err(ev.error(
            ErrorKind::Eval,
            "in pure evaluation mode, 'fetchMercurial' requires a Mercurial revision",
        ));
    };
    let mut attrs: BTreeMap<String, FetchAttr> = BTreeMap::new();
    attrs.insert("type".into(), FetchAttr::String("hg".into()));
    let url = if url.contains("://") {
        url
    } else {
        format!("file://{url}")
    };
    attrs.insert("url".into(), FetchAttr::String(url));
    attrs.insert("name".into(), FetchAttr::String(name));
    attrs.insert("rev".into(), FetchAttr::String(rev));
    let tree = ev
        .host
        .fetch_tree(&FetchRequest {
            attrs,
            who: "fetchMercurial",
            empty_rev_fallback: false,
            nar_hash_is_final: false,
            // `Input::fetchToStore`, past the input cache (see the field).
            refetch: true,
        })
        .map_err(|e| fetch_failed(ev, e))?;
    let sp = mount_tree(ev, &tree)?;
    let mut result: Vec<Attr<'a>> = Vec::with_capacity(5);
    result.push(ev.attr(sym::OUT_PATH, ev.store_path_string(&sp)));
    if let Some(FetchAttr::String(branch)) = tree.locked_attrs.get("ref") {
        result.push(ev.attr(ev.sym("branch"), ev.mk_string(branch)));
    }
    // "Backward compatibility": the all-zero revision for a working tree
    // with uncommitted changes. The fetcher writes a revision as 40 hex
    // digits.
    let rev = match tree.locked_attrs.get("rev") {
        Some(FetchAttr::String(rev)) => rev.clone(),
        _ => "0".repeat(40),
    };
    let short = rev.get(..12).unwrap_or(rev.as_str());
    result.push(ev.attr(ev.sym("shortRev"), ev.mk_string(short)));
    result.push(ev.attr(ev.sym("rev"), ev.mk_string(&rev)));
    if let Some(FetchAttr::Int(n)) = tree.locked_attrs.get("revCount") {
        // `mkInt` of Nix's `uint64_t`.
        result.push(ev.attr(ev.sym("revCount"), Value::Int(*n as i64)));
    }
    Ok(ev.mk_attrs(result, SrcPos::NONE))
}

/// `fetchClosure` (`primops/fetchClosure.cc`) copies store paths from a
/// binary cache; brix has no store to copy them into.
fn prim_fetch_closure<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    _args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    Err(ev.error_at(
        ErrorKind::Unsupported,
        "brix does not implement 'builtins.fetchClosure'",
        pos,
    ))
}
