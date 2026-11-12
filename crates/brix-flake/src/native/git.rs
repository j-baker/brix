//! `git` inputs (`src/libfetchers/git.cc`), implemented with `gix`.
//!
//! Nix ≥ 2.20 exports a revision by walking the Git tree objects
//! directly (no `git archive`, so filters and `export-subst` do not
//! apply); so does brix. With `exportIgnore` the paths the
//! `export-ignore` attribute names are hidden, the attribute read as
//! libgit2 reads it ([`super::git_attrs`]). With `nix-219-compat` a
//! commit is exported as Nix 2.19 did, by `git archive` or `git checkout`
//! ([`super::git_legacy`]), and either export is tried when a locked
//! `narHash` was made by the other. Remote repositories are fetched into
//! a bare clone under the cache directory, keyed by the URL like Nix's
//! `~/.cache/nix/gitv3/<sha256 of url>`, by running `git` as Nix runs it
//! (with gix when there is no `git`, as Nix falls back to libgit2); local
//! repositories are opened in place.
//!
//! A local working tree without `ref` or `rev` is not exported at all:
//! like Nix, brix reads it in place, uncommitted changes included (see
//! [`super::git_workdir`] and [`fetch_workdir`]).
//!
//! A gitlink is an empty directory unless the input asks for
//! `submodules`: then each submodule `.gitmodules` lists is fetched as an
//! input of its own (a commit's at its gitlink's revision, with its URL
//! resolved against the parent's, see [`super::git_submodules`]; a
//! working tree's from its checkout) and mounted there.
//!
//! Scope: a revision (`rev`), a branch/tag (`ref`), a local working tree
//! (clean or dirty), `lastModified`, `revCount`, `submodules`,
//! `exportIgnore`, Nix 2.19 exports and commit signatures
//! (`verifyCommit`, see [`super::git_verify`]) and Git LFS files (`lfs`,
//! smudged in an exported commit, see [`super::git_lfs`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brix_store::accessor::{AccessorError, DirEntries, MemoryNode, Stat};
use brix_store::{
    nar, BStr, BString, CanonPath, Hash, HashAlgo, HashFormat, MemoryAccessor, MountedAccessor,
    SourceAccessor,
};

use super::cache::FetchCache;
use super::git_attrs::{AttrSources, ExportIgnoreAccessor, GitAttributes};
use super::git_workdir::{Workdir, WorkdirAccessor};
use super::http::HttpClient;
use crate::attrs::{get_str_attr, maybe_get_bool_attr, Attr, Attrs};
use crate::fetch::FetchedTree;
use crate::hashes::{GitRev, NarHash};
use crate::input::{FetchSettings, Input, Scheme};
use crate::{err, Result};
use brix_store::url::{parse_url, url_path_to_path};
use gix::protocol::transport::client::Transport;

/// Where the bare clone of `url` lives: `getCachePath(url, shallow)`.
pub fn cache_path(cache_dir: &Path, url: &str, shallow: bool) -> PathBuf {
    let name = Hash::of(HashAlgo::Sha256, url.as_bytes()).to_string(HashFormat::Nix32, false);
    cache_dir.join("gitv3").join(if shallow {
        format!("{name}-shallow")
    } else {
        name
    })
}

/// Holds the bare clone at `dir` for one fetch into it (`PathLocks
/// cacheDirLock({cacheDir})`: a lock on `<dir>.lock`). Inputs are fetched
/// in parallel, by the threads of one process and by brix processes
/// sharing the cache, and two revisions of one repository share a clone,
/// so creating it or updating its refs from two at once would race.
fn lock_clone(dir: &Path) -> Result<super::lock::FileLock> {
    let parent = dir.parent().expect("invariant: a clone lives in gitv3/");
    std::fs::create_dir_all(parent).map_err(|e| crate::Error::Io {
        context: format!("creating '{}'", parent.display()),
        source: e,
    })?;
    let mut name = dir.as_os_str().to_owned();
    name.push(".lock");
    super::lock::FileLock::acquire(Path::new(&name))
}

/// `RepoInfo::location`: a local non-bare repository path, or a URL to
/// fetch from.
enum Location {
    Local(PathBuf),
    Remote(String),
}

fn location(input: &Input) -> Result<Location> {
    // `fixGitURL` normalised the `url` attribute when the input was made,
    // so it has no query parameter to warn about.
    let url = parse_url(get_str_attr(&input.attrs, "url")?, false, &|_| {})?;
    if url.scheme == "file" {
        let path = url_path_to_path(&url.path)?;
        let p = PathBuf::from(&path);
        let bare = p.exists() && !p.join(".git").exists();
        if !bare {
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .map_err(|e| err!("getting the current directory: {}", e))?
                    .join(p)
            };
            return Ok(Location::Local(abs));
        }
        let mut url = url;
        url.query.clear();
        return Ok(Location::Remote(url.to_string()));
    }
    let mut url = url;
    url.query.remove("dir");
    Ok(Location::Remote(url.to_string()))
}

/// `GitInputScheme::getSourcePath`: the working tree of a local,
/// non-bare repository.
pub fn source_path(input: &Input) -> Result<Option<PathBuf>> {
    match location(input)? {
        Location::Local(path) => Ok(Some(path)),
        Location::Remote(_) => Ok(None),
    }
}

fn gix_err(context: &str, e: impl std::fmt::Display) -> crate::Error {
    err!("{}: {}", context, e)
}

/// [`gix_err`] for an error from talking to a remote, with its causes:
/// gix's own message there ("An IO error occurred when talking to the
/// server") does not say what went wrong.
fn remote_err(context: &str, e: impl std::error::Error) -> crate::Error {
    let mut message = format!("{context}: {e}");
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    err!("{}", message)
}

/// Opens the repository of a local checkout.
pub fn open_local(path: &Path) -> Result<gix::Repository> {
    gix::open(path).map_err(|e| gix_err(&format!("opening Git repository '{}'", path.display()), e))
}

/// The working tree a `git` input names when it is a local checkout with
/// neither `ref` nor `rev`. Nix reads such an input in place
/// (`getAccessorFromWorkdir`) rather than exporting a commit, so it is
/// never served from a cache by reference.
pub fn local_workdir(input: &Input) -> Result<Option<PathBuf>> {
    if input.scheme != Some(Scheme::Git) || input.ref_()?.is_some() || input.rev()?.is_some() {
        return Ok(None);
    }
    match location(input)? {
        Location::Local(path) => Ok(Some(path)),
        Location::Remote(_) => Ok(None),
    }
}

/// Whether a `git` input is a local checkout, whose references Nix
/// resolves in the repository on every fetch instead of remembering them
/// for `tarball-ttl`.
pub fn is_local_checkout(input: &Input) -> Result<bool> {
    Ok(input.scheme == Some(Scheme::Git) && matches!(location(input)?, Location::Local(_)))
}

/// For a local checkout without a `ref`: the one `getAccessorFromCommit`
/// gives it (`getDefaultRef`), the branch checked out now, `HEAD` when
/// detached. A fetch answered from the cache needs it too: the branch is
/// not a property of the commit, so the cache does not remember it.
pub fn local_default_ref(input: &Input) -> Result<Option<String>> {
    if input.ref_()?.is_some() || !is_local_checkout(input)? {
        return Ok(None);
    }
    let Location::Local(path) = location(input)? else {
        return Ok(None);
    };
    let repo = open_local(&path)?;
    Ok(Some(
        workdir_ref(&repo)?.unwrap_or_else(|| "HEAD".to_owned()),
    ))
}

/// For a local checkout with a `ref` but no `rev`: the revision the ref
/// points to now.
pub fn resolve_local_ref(input: &Input) -> Result<Option<GitRev>> {
    if input.rev()?.is_some() || !is_local_checkout(input)? {
        return Ok(None);
    }
    let (Some(ref_), Location::Local(path)) = (input.ref_()?, location(input)?) else {
        return Ok(None);
    };
    resolve_ref(&open_local(&path)?, ref_).map(Some)
}

fn check_supported(input: &Input) -> Result<()> {
    // `GitInputScheme::getAccessor`: there is no Git behaviour to copy
    // (`git archive` knows no submodules), so Nix refuses the two together.
    if wants_export_ignore(input)? && wants_submodules(input)? {
        return Err(err!(
            "exportIgnore and submodules are not supported together yet"
        ));
    }
    Ok(())
}

/// `getSubmodulesAttr`: whether each submodule is fetched and mounted at
/// its gitlink, which is otherwise an empty directory.
fn wants_submodules(input: &Input) -> Result<bool> {
    Ok(maybe_get_bool_attr(&input.attrs, "submodules")?.unwrap_or(false))
}

/// `getExportIgnoreAttr`: whether what `export-ignore` names is hidden.
fn wants_export_ignore(input: &Input) -> Result<bool> {
    Ok(maybe_get_bool_attr(&input.attrs, "exportIgnore")?.unwrap_or(false))
}

/// `getLfsAttr`.
fn wants_lfs(input: &Input) -> Result<bool> {
    Ok(maybe_get_bool_attr(&input.attrs, "lfs")?.unwrap_or(false))
}

/// Whether brix's fetch cache must not answer for `input` from its
/// attributes: a commit whose tree depends on more than they say
/// (`nix-219-compat` exports it as Nix 2.19 did), or whose fingerprint
/// does (`;s` when its submodules are mounted, which only the export
/// knows). Nix exports such a commit again on every fetch too; only a
/// tree whose `narHash` is known is found by that.
pub fn bypasses_fetch_cache(settings: &FetchSettings, input: &Input) -> Result<bool> {
    if input.scheme != Some(Scheme::Git) || local_workdir(input)?.is_some() {
        return Ok(false);
    }
    Ok(wants_submodules(input)? || (settings.nix219_compat && !wants_lfs(input)?))
}

/// Whether reading `workdir` for `input` mounts its submodules' own
/// working trees, whose contents its status does not cover: a clean
/// tree with them is not described by its revision alone.
pub fn mounts_workdir_submodules(input: &Input, workdir: &Workdir) -> Result<bool> {
    Ok(wants_submodules(input)? && !workdir.info.submodules.is_empty())
}

/// What fetching a Git input needs besides the input itself: the client
/// and cache directory remote repositories (submodules among them) are
/// fetched with, the settings, where warnings go, the fetch cache that
/// remembers NAR hashes by fingerprint (see [`remembered_nar_hash`]), if
/// there is one, and whether repositories to fetch from are asked with
/// `git`.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    http: &'a HttpClient,
    cache_dir: &'a Path,
    settings: &'a FetchSettings,
    warn: &'a dyn Fn(&str),
    hashes: Option<&'a FetchCache>,
    /// Nix runs `git` for all it asks of a repository to fetch from (its
    /// `HEAD`, the fetch, the clone's remembered `HEAD`), passing on what
    /// git prints, and fetches with libgit2 only when there is no `git`
    /// on `PATH`. brix does the same ([`git_on_path`]), with gix in
    /// libgit2's place, so that with `git` the messages, and the
    /// protocols, proxies, credentials and certificates git handles, are
    /// Nix's.
    git: bool,
}

/// `GitInputScheme::getAccessor`, without any caching.
pub fn fetch(
    http: &HttpClient,
    cache_dir: &Path,
    settings: &FetchSettings,
    input: &Input,
    warn: &dyn Fn(&str),
) -> Result<FetchedTree> {
    fetch_cached(http, cache_dir, None, settings, input, warn)
}

/// [`fetch`], with the NAR hashes Nix's `fetchToStore` remembers by
/// fingerprint kept in (and taken from) `hashes`.
pub fn fetch_cached(
    http: &HttpClient,
    cache_dir: &Path,
    hashes: Option<&FetchCache>,
    settings: &FetchSettings,
    input: &Input,
    warn: &dyn Fn(&str),
) -> Result<FetchedTree> {
    let ctx = Ctx {
        http,
        cache_dir,
        settings,
        warn,
        hashes,
        git: git_on_path(),
    };
    get_accessor(&ctx, input, None, true)
}

/// `Input::getAccessorUnchecked`'s two tries: Nix asks the scheme first
/// for what it has without fetching (`fastOnly`), drops whatever error
/// that try ends in, and then fetches. Whatever the first try prints (a
/// warning, `git`'s own messages) is printed again when it fails or stops
/// short of a fetch; a working tree and a local repository are read in
/// full by the first try already.
fn get_accessor(
    ctx: &Ctx<'_>,
    input: &Input,
    display: Option<String>,
    count_revs: bool,
) -> Result<FetchedTree> {
    // `None` means the tree needs fetching; an error is dropped (Nix's
    // `catch (...) {}`), and the second try fails again, or not.
    if let Ok(Some(tree)) = fetch_in(ctx, input, display.clone(), count_revs, true) {
        return Ok(tree);
    }
    Ok(fetch_in(ctx, input, display, count_revs, false)?
        .expect("invariant: a try that may fetch returns a tree"))
}

/// One try of [`get_accessor`]: of a tree's own input, or of one of its
/// submodules (`display` is then how its paths are shown, and its
/// `revCount`, which Nix computes lazily and nobody looks at, is not
/// computed: `count_revs` is false). With `fast_only`, `None` when the
/// commit must be fetched first.
fn fetch_in(
    ctx: &Ctx<'_>,
    input: &Input,
    display: Option<String>,
    count_revs: bool,
    fast_only: bool,
) -> Result<Option<FetchedTree>> {
    check_supported(input)?;
    if let Some(path) = local_workdir(input)? {
        let workdir = Workdir::open(&path)?;
        return fetch_workdir_in(ctx, input, &workdir, None, count_revs, display).map(Some);
    }
    fetch_commit(ctx, input, display, count_revs, fast_only)
}

/// `getAccessorFromCommit`: the commit `input` names (by `rev`, by `ref`,
/// or the default branch) and its tree (see [`commit_tree`]), shown as
/// `display` or else as the locked input. With `fast_only`, `None` when
/// the clone of a remote repository must fetch first.
fn fetch_commit(
    ctx: &Ctx<'_>,
    input: &Input,
    display: Option<String>,
    count_revs: bool,
    fast_only: bool,
) -> Result<Option<FetchedTree>> {
    let Ctx {
        http,
        cache_dir,
        warn,
        ..
    } = *ctx;
    let mut input = input.clone();
    let shallow = maybe_get_bool_attr(&input.attrs, "shallow")?.unwrap_or(false)
        || input.rev_count()?.is_some();
    let all_refs = maybe_get_bool_attr(&input.attrs, "allRefs")?.unwrap_or(false);

    // `getAccessorFromCommit`: the input always ends up with a `ref`, the
    // one it names or the repository's default branch.
    let original_ref = input.ref_()?.map(str::to_owned);
    let location = location(&input)?;
    let (repo, rev) = match &location {
        Location::Local(path) => {
            let repo = open_local(path)?;
            // `getDefaultRef`: the checked-out branch, `HEAD` when detached.
            // (A checkout with neither `ref` nor `rev` went to
            // `fetch_workdir` above.)
            let ref_ = match original_ref {
                Some(r) => r,
                None => local_default_ref(&input)?
                    .expect("invariant: a local checkout without a ref has a default one"),
            };
            let rev = match input.rev()? {
                Some(rev) => rev,
                None => resolve_ref(&repo, &ref_)?,
            };
            input.attrs.insert("ref".into(), Attr::String(ref_));
            (repo, rev)
        }
        Location::Remote(url) => {
            let dir = cache_path(cache_dir, url, shallow);
            let _clone = lock_clone(&dir)?;
            let repo = open_or_init_bare(&dir)?;
            let ref_ = match &original_ref {
                Some(r) => r.clone(),
                None => default_remote_ref(ctx, &repo, url)?,
            };
            input.attrs.insert("ref".into(), Attr::String(ref_.clone()));
            let rev = input.rev()?;
            // `localRefFile`: where the clone keeps the ref, whose age
            // says whether it is fetched again.
            let local_ref_file = if ref_.starts_with("refs/") {
                dir.join(&ref_)
            } else {
                dir.join("refs/heads").join(&ref_)
            };
            let now = unix_now();
            // A revision the clone has needs no fetch, nor a ref fetched
            // less than `tarball-ttl` ago; every ref (`allRefs`) does.
            let do_fetch = match rev {
                Some(rev) => !repo.has_object(oid(&rev)),
                None => all_refs || !within_ttl(&local_ref_file, ctx.settings.tarball_ttl, now)?,
            };
            if do_fetch {
                if fast_only {
                    return Ok(None);
                }
                let refspec = fetch_refspec(all_refs, rev, &ref_);
                let fetched = if ctx.git {
                    fetch_with_git(&dir, url, &refspec, shallow)
                } else {
                    fetch_remote(http, &repo, url, &refspec, shallow)
                };
                // A failed fetch of a ref the clone has is reported, and
                // the version the clone has is taken.
                if let Err(e) = fetched {
                    let have = local_ref_file.try_exists().map_err(|io| crate::Error::Io {
                        context: format!("checking '{}'", local_ref_file.display()),
                        source: io,
                    })?;
                    if !have {
                        return Err(e);
                    }
                    (ctx.settings.error)(&e.to_string());
                    warn(&format!(
                        "could not update local clone of Git repository '{url}'; continuing with the most recent version"
                    ));
                }
                if rev.is_none() {
                    if let Err(e) = set_write_time(&local_ref_file, now) {
                        warn(&format!(
                            "could not update mtime for file \"{}\": {e}",
                            local_ref_file.display()
                        ));
                    }
                }
                if original_ref.is_none() {
                    store_cached_head(ctx.git, &dir, &repo, &ref_, url, warn)?;
                }
            }
            let rev = match rev {
                Some(rev) if !repo.has_object(oid(&rev)) => {
                    return Err(err!(
                        "Cannot find Git revision '{}' in ref '{}' of repository '{}'! Please make sure that the \x1b[1mrev\x1b[0m exists on the \x1b[1mref\x1b[0m you've specified or add \x1b[1mallRefs = true;\x1b[0m to \x1b[1mfetchGit\x1b[0m.",
                        rev,
                        ref_,
                        url
                    ))
                }
                Some(rev) => rev,
                None => resolve_ref(&repo, &ref_)?,
            };
            (repo, rev)
        }
    };

    input.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
    let commit = find_commit(&repo, &rev)?;

    if !input.attrs.contains_key("lastModified") {
        input
            .attrs
            .insert("lastModified".into(), Attr::Int(commit_time(&commit)?));
    }
    if !shallow && !input.attrs.contains_key("revCount") {
        if repo.is_shallow() {
            return Err(err!(
                "'{}' is a shallow Git repository, but shallow repositories are only allowed when `shallow = true;` is specified",
                input
            ));
        }
        if count_revs {
            input
                .attrs
                .insert("revCount".into(), Attr::Int(rev_count(&repo, &commit)?));
        }
    }

    // `verifyCommit(input, repo)`: the signature, before the tree is used.
    super::git_verify::verify(&input, repo.workdir().unwrap_or(repo.path()))?;

    let source = CommitSource {
        repo: &repo,
        location: &location,
        commit: &commit,
        rev,
    };
    let (tree, nar_hash) = commit_tree(ctx, &source, &input, display)?;
    let verified = match nar_hash {
        Some(hash) => {
            input
                .attrs
                .insert("narHash".into(), Attr::String(hash.to_sri()));
            true
        }
        None => false,
    };
    Ok(Some(FetchedTree {
        accessor: tree,
        input,
        verified,
    }))
}

/// A commit to export, and the repository it is in.
struct CommitSource<'a, 'r> {
    repo: &'r gix::Repository,
    location: &'a Location,
    commit: &'a gix::Commit<'r>,
    rev: GitRev,
}

/// The tree `getAccessorFromCommit` returns: `getTree(useLegacy)`, the
/// export of Nix ≥ 2.20, or of Nix 2.19 under `nix-219-compat` (a
/// submodule follows its parent, `__legacyExport`). When the input
/// expects a `narHash` the tree does not have, the other export is tried
/// and used if it has it (a lock made by the other Nix), with a warning
/// when that is Nix 2.19's. Each tree's hash is computed as Nix's
/// `fetchToStore` computes it, through the hashes it remembers by
/// fingerprint ([`nar_hash_of`]). The NAR hash comes back when it was
/// checked and is the tree's own (not a remembered one, which the tree
/// need not have: the fetch cache files trees by what comes back).
fn commit_tree(
    ctx: &Ctx<'_>,
    source: &CommitSource<'_, '_>,
    input: &Input,
    display: Option<String>,
) -> Result<(Arc<dyn SourceAccessor>, Option<NarHash>)> {
    let legacy_export = maybe_get_bool_attr(&input.attrs, "__legacyExport")?;
    let use_legacy = legacy_export.unwrap_or(ctx.settings.nix219_compat) && !wants_lfs(input)?;
    if use_legacy && legacy_export.is_none() {
        (ctx.warn)(&format!(
            "Using Nix 2.19 semantics to export Git repository '{}'.",
            input.to_url_text(false)?
        ));
    }
    let display = match display {
        Some(display) => display,
        None => format!("«{}»", input.to_url_text(true)?),
    };
    let tree = export(ctx, source, input, use_legacy, &display)?;
    let Some(expected) = input.nar_hash()? else {
        return Ok((tree, None));
    };
    let (hash, computed) = nar_hash_of(ctx, &tree)?;
    if hash == expected {
        return Ok((tree, computed.then_some(hash)));
    }
    let other = export(ctx, source, input, !use_legacy, &display)?;
    let (other_hash, other_computed) = nar_hash_of(ctx, &other)?;
    if other_hash != expected {
        // The mismatch is reported for the first tree.
        return Ok((tree, None));
    }
    let computed = other_computed;
    if !use_legacy {
        (ctx.warn)(&format!(
            "Git input '{}' specifies a NAR hash '{}' that was created by Nix < 2.20.\nNix >= 2.20 does not apply Git filters, `export-ignore` and `export-subst` by default, which changes the NAR hash.\nPlease update the NAR hash to '{}'.",
            input.to_url_text(false)?,
            expected.to_sri(),
            hash.to_sri()
        ));
    }
    Ok((other, computed.then_some(expected)))
}

/// `fetchToStore2(…, DryRun)` of a whole exported tree: the NAR hash
/// remembered for its fingerprint, or the tree's own, computed and then
/// remembered (see [`remembered_nar_hash`]). Says whether it was computed.
fn nar_hash_of(ctx: &Ctx<'_>, tree: &Arc<dyn SourceAccessor>) -> Result<(NarHash, bool)> {
    let remembering = ctx.hashes.zip(tree.fingerprint());
    if let Some((cache, fingerprint)) = remembering {
        if let Some(hash) = remembered_nar_hash(cache, fingerprint)? {
            return Ok((hash, false));
        }
    }
    let (hash, _) = nar::hash(tree.as_ref(), &CanonPath::root(), HashAlgo::Sha256)?;
    let hash = nar_hash_from(&hash);
    if let Some((cache, fingerprint)) = remembering {
        remember_nar_hash(cache, fingerprint, &hash)?;
    }
    Ok((hash, true))
}

/// A SHA-256 [`Hash`] as a [`NarHash`].
fn nar_hash_from(hash: &Hash) -> NarHash {
    NarHash::from_bytes(
        hash.as_bytes()
            .try_into()
            .expect("invariant: NAR hashes are SHA-256"),
    )
}

/// Nix's `sourcePathToHash` cache, which `fetchToStore` answers from
/// before it hashes a tree (`makeSourcePathToHashCacheKey(fingerprint,
/// NixArchive, /)`): the NAR hash remembered for trees with
/// `fingerprint`. It is the first such tree's: a fingerprint that does not
/// tell all (a working tree's, `git:<rev>`, is the same whether its
/// submodules are mounted or not, and the same as the commit's without
/// them) gets that tree's hash whatever the tree at hand holds, as in Nix.
pub fn remembered_nar_hash(cache: &FetchCache, fingerprint: &str) -> Result<Option<NarHash>> {
    let Some(value) = cache.lookup(&super::cache::source_path_hash_key(fingerprint), None)? else {
        return Ok(None);
    };
    let hash = NarHash::parse_sri(get_str_attr(&value, "hash")?, Default::default())?;
    Ok(Some(hash))
}

/// Remembers `hash` for trees with `fingerprint` (see
/// [`remembered_nar_hash`]).
pub fn remember_nar_hash(cache: &FetchCache, fingerprint: &str, hash: &NarHash) -> Result<()> {
    let mut value = Attrs::new();
    value.insert("hash".into(), Attr::String(hash.to_sri()));
    cache.insert(&super::cache::source_path_hash_key(fingerprint), &value)
}

/// `getTree(legacy)`: the commit exported as Nix ≥ 2.20 does (its Git
/// tree, without what `export-ignore` names under `exportIgnore`) or as
/// Nix 2.19 did ([`super::git_legacy`], which leaves out what
/// `export-ignore` names unless it checks the commit out for its
/// submodules), with the submodules mounted when the input asks for them.
/// The tree carries the fingerprint Nix's accessor for it has
/// (`GitAccessorOptions::makeFingerprint`, then `;s` when submodules were
/// mounted).
fn export(
    ctx: &Ctx<'_>,
    source: &CommitSource<'_, '_>,
    input: &Input,
    legacy: bool,
    display: &str,
) -> Result<Arc<dyn SourceAccessor>> {
    let submodules = wants_submodules(input)?;
    let lfs = wants_lfs(input)?;
    let export_ignore = if legacy {
        !submodules
    } else {
        wants_export_ignore(input)?
    };
    let mut fingerprint = format!(
        "git:{}{}{}{}",
        source.rev.to_hex(),
        if export_ignore { ";e" } else { "" },
        if lfs { ";l" } else { "" },
        if legacy { ";legacy" } else { "" }
    );
    let tree: Arc<dyn SourceAccessor> = if legacy {
        // `git -C <repo> --git-dir <git dir> …`: a checkout's `.git`, or
        // the bare clone itself.
        let (repo_dir, git_dir) = match source.location {
            Location::Local(path) => (path.as_path(), ".git"),
            Location::Remote(_) => (source.repo.git_dir(), "."),
        };
        let mut tree =
            super::git_legacy::export(repo_dir, git_dir, &source.rev, submodules, ctx.warn)?;
        tree.set_display_prefix(display);
        Arc::new(tree)
    } else {
        let (mut tree, attribute_files) = export_tree(source.repo, source.commit)?;
        tree.set_display_prefix(display);
        let mut tree: Arc<dyn SourceAccessor> = Arc::new(tree);
        // The attributes `export-ignore` and LFS's `filter` are read from.
        let attrs = if export_ignore || lfs {
            let sources = AttrSources::for_commit(source.repo, attribute_files)?;
            Some(Arc::new(GitAttributes::new(sources)))
        } else {
            None
        };
        if let (true, Some(attrs)) = (lfs, &attrs) {
            // `GitSourceAccessor::readBlob` smudges an LFS pointer when it
            // is read.
            let remote = match source.location {
                Location::Remote(url) => Some(url.as_str()),
                Location::Local(_) => None,
            };
            let smudge = super::git_lfs::Smudge {
                http: ctx.http,
                cache_dir: ctx.cache_dir,
                settings: ctx.settings,
                warn: ctx.warn,
            };
            tree = Arc::new(smudge.accessor(source.repo, remote, tree, attrs.clone())?);
        }
        if let (true, Some(attrs)) = (export_ignore, attrs) {
            tree = Arc::new(ExportIgnoreAccessor::new(tree, attrs, Vec::new()));
        }
        tree
    };
    if submodules {
        let mounts = commit_submodules(ctx, source, input, export_ignore, lfs, legacy)?;
        if !mounts.is_empty() {
            let mut mounted = MountedAccessor::new(tree);
            for (path, submodule) in mounts {
                mounted.mount(path, submodule);
            }
            fingerprint.push_str(";s");
            return Ok(with_fingerprint(Arc::new(mounted), fingerprint, ctx.hashes));
        }
    }
    Ok(with_fingerprint(tree, fingerprint, ctx.hashes))
}

/// The submodules of a commit to mount, each fetched as an input of its
/// own: at its gitlink's revision, from its URL resolved against the
/// parent's (a remote's, or a checkout's remote or directory), with
/// `allRefs`, `submodules` and the parent's `exportIgnore`, `lfs` and
/// export semantics (`__legacyExport`), and shown as that input.
fn commit_submodules(
    ctx: &Ctx<'_>,
    source: &CommitSource<'_, '_>,
    input: &Input,
    export_ignore: bool,
    lfs: bool,
    legacy: bool,
) -> Result<Vec<(CanonPath, Arc<dyn SourceAccessor>)>> {
    let submodules = super::git_submodules::at_commit(source.repo, source.commit)?;
    if submodules.is_empty() {
        return Ok(Vec::new());
    }
    let base = match source.location {
        Location::Local(path) => super::git_submodules::local_url_base(source.repo, path)?,
        // Nix makes the clone's `origin` the URL it fetches from, which
        // libgit2 then rewrites by `url.<base>.insteadOf`.
        Location::Remote(url) => super::git_submodules::apply_insteadof(source.repo, url)?,
    };
    let parent_ref = input
        .ref_()?
        .expect("invariant: getAccessorFromCommit gave the input a ref")
        .to_owned();
    let mut mounts = Vec::new();
    for (submodule, rev) in submodules {
        let url = super::git_submodules::resolve_url(&submodule.url, &base)?;
        let mut attrs = Attrs::new();
        attrs.insert("type".into(), Attr::String("git".into()));
        attrs.insert("url".into(), Attr::String(url));
        if !submodule.branch.is_empty() {
            // `.` means the parent's branch (gitmodules(5)).
            let ref_ = if submodule.branch == "." {
                parent_ref.clone()
            } else {
                submodule.branch.clone()
            };
            attrs.insert("ref".into(), Attr::String(ref_));
        }
        attrs.insert("rev".into(), Attr::String(rev.to_hex()));
        attrs.insert("exportIgnore".into(), Attr::Bool(export_ignore));
        attrs.insert("submodules".into(), Attr::Bool(true));
        attrs.insert("lfs".into(), Attr::Bool(lfs));
        attrs.insert("allRefs".into(), Attr::Bool(true));
        attrs.insert("__legacyExport".into(), Attr::Bool(legacy));
        let submodule_input = Input::from_attrs(ctx.settings, attrs)?;
        let display = format!("«{}»", submodule_input.to_url_text(true)?);
        let fetched = get_accessor(ctx, &submodule_input, Some(display), false)
            .map_err(|e| e.trace(format!("while fetching the input '{submodule_input}'")))?;
        mounts.push((submodule.path, fetched.accessor));
    }
    Ok(mounts)
}

/// `accessor` with the fingerprint Nix's accessor for the same tree has
/// (an exported commit's, a clean working tree's, or the scheme's for a
/// tree the fetch cache gives back), its NAR hash remembered by that
/// fingerprint in `hashes` (see [`remembered_nar_hash`]).
pub fn with_fingerprint(
    accessor: Arc<dyn SourceAccessor>,
    fingerprint: String,
    hashes: Option<&FetchCache>,
) -> Arc<dyn SourceAccessor> {
    Arc::new(GitTree {
        inner: accessor,
        fingerprint,
        hashes: hashes.cloned(),
    })
}

/// A Git tree with the fingerprint Nix's accessor for it has, which the
/// fingerprint of a flake in it starts from (see
/// [`crate::fingerprint::input_fingerprint`]) and by which Nix's
/// `fetchToStore` remembers its NAR hash.
struct GitTree {
    inner: Arc<dyn SourceAccessor>,
    fingerprint: String,
    /// Where the NAR hash is remembered, if anywhere.
    hashes: Option<FetchCache>,
}

impl GitTree {
    /// The whole tree's NAR hash as Nix's `fetchToStore` computes it:
    /// remembered by fingerprint, or computed and then remembered (see
    /// [`remembered_nar_hash`]). An error of the tree's own keeps its
    /// traces (a Git LFS file that cannot be smudged); one of the cache is
    /// told as it is.
    fn remembered_or_hashed(
        &self,
        hashes: &FetchCache,
    ) -> std::result::Result<NarHash, AccessorError> {
        let cache_err = |e: crate::Error| AccessorError::Io(e.to_string().into());
        if let Some(hash) = remembered_nar_hash(hashes, &self.fingerprint).map_err(cache_err)? {
            return Ok(hash);
        }
        let (own, _) = nar::hash(self.inner.as_ref(), &CanonPath::root(), HashAlgo::Sha256)
            .map_err(|e| match e {
                nar::NarError::Accessor(e) => e,
                other => AccessorError::Io(other.message_bytes().into()),
            })?;
        let own = nar_hash_from(&own);
        remember_nar_hash(hashes, &self.fingerprint, &own).map_err(cache_err)?;
        Ok(own)
    }
}

impl SourceAccessor for GitTree {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        self.inner.maybe_lstat(path)
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        self.inner.lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        self.inner.read_file(path)
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> std::result::Result<u64, AccessorError> {
        self.inner.read_file_to(path, out)
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        self.inner.read_link(path)
    }

    fn display_prefix(&self) -> &BStr {
        self.inner.display_prefix()
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        self.inner.show_path(path)
    }

    fn physical_path(&self, path: &CanonPath) -> Option<PathBuf> {
        self.inner.physical_path(path)
    }

    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(Hash, u64)> {
        self.inner.precomputed_nar_hash(path)
    }

    /// The whole tree's NAR hash as Nix's `fetchToStore` has it, when the
    /// evaluator copies it to the store (a lazily mounted tree's store
    /// path): remembered by fingerprint, so possibly another tree's, as in
    /// Nix. Only the whole tree's is remembered.
    fn remembered_nar_hash(
        &self,
        path: &CanonPath,
    ) -> std::result::Result<Option<Hash>, AccessorError> {
        let (Some(hashes), true) = (&self.hashes, path.is_root()) else {
            return Ok(None);
        };
        let hash = self.remembered_or_hashed(hashes)?;
        Ok(Some(Hash::from_bytes(HashAlgo::Sha256, hash.as_bytes())))
    }

    fn fingerprint(&self) -> Option<&str> {
        Some(&self.fingerprint)
    }

    fn mounted_show_path(&self, path: &CanonPath) -> Option<BString> {
        self.inner.mounted_show_path(path)
    }
}

/// `GitInputScheme::getAccessorFromWorkdir`: the tracked files of a local
/// working tree, read from disk.
///
/// A clean tree is a checkout of `HEAD` and gets its `rev`, `revCount` and
/// branch (`ref`). A dirty one gets none of them: it is described by
/// `dirtyRev` (`HEAD` with a `-dirty` suffix), is an error under
/// `allow-dirty = false` and is warned about under `warn-dirty`.
///
/// Each gitlink is an empty directory, unless the input asks for
/// `submodules` and `.gitmodules` gives the submodule a URL: then the
/// submodule's own checkout is read as an input of its own (with
/// `submodules` and the parent's `exportIgnore`) and mounted there, and a
/// dirty submodule makes the tree dirty.
///
/// `known` is what an earlier fetch of the same clean revision found:
/// its `narHash` (if it has one), `lastModified` and `revCount` are taken
/// as they are instead of being computed again. It must not be given for
/// a tree whose submodules are mounted ([`mounts_workdir_submodules`]). A
/// clean tree has the fingerprint `git:<rev>` (with `;e`, `;l`), by which
/// its NAR hash is remembered in `hashes` (see [`remembered_nar_hash`]).
///
/// As for [`get_accessor`], a tree that fails to be read is read again
/// (Nix's `fastOnly` try reads a working tree in full), so that what the
/// first try printed is printed twice.
#[allow(clippy::too_many_arguments)]
pub fn fetch_workdir(
    http: &HttpClient,
    cache_dir: &Path,
    hashes: Option<&FetchCache>,
    settings: &FetchSettings,
    input: &Input,
    workdir: &Workdir,
    known: Option<&Attrs>,
    warn: &dyn Fn(&str),
) -> Result<FetchedTree> {
    let ctx = Ctx {
        http,
        cache_dir,
        settings,
        warn,
        hashes,
        git: git_on_path(),
    };
    // The error of the `fastOnly` try is dropped (Nix's `catch (...) {}`).
    fetch_workdir_in(&ctx, input, workdir, known, true, None)
        .or_else(|_| fetch_workdir_in(&ctx, input, workdir, known, true, None))
}

/// [`fetch_workdir`] in a context, once; a submodule's `revCount` is not
/// computed (`count_revs`), as for [`fetch_in`], and its files are shown
/// as `display` followed by their path on disk (`setPathDisplay` on the
/// submodule's accessor, whose own paths are its files' on disk).
fn fetch_workdir_in(
    ctx: &Ctx<'_>,
    input: &Input,
    workdir: &Workdir,
    known: Option<&Attrs>,
    count_revs: bool,
    display: Option<String>,
) -> Result<FetchedTree> {
    let Ctx { settings, warn, .. } = *ctx;
    check_supported(input)?;
    let Workdir { path, repo, info } = workdir;
    let export_ignore = wants_export_ignore(input)?;
    let mut is_dirty = info.is_dirty;
    let mut mounts: Vec<(CanonPath, Arc<dyn SourceAccessor>)> = Vec::new();
    if wants_submodules(input)? && !info.submodules.is_empty() {
        // `.gitmodules` only supplies the URLs: the gitlinks of the index
        // are the submodules.
        let configured = super::git_submodules::workdir_gitmodules(path)?;
        for gitlink in &info.submodules {
            let has_url = configured
                .iter()
                .find(|submodule| &submodule.path == gitlink)
                .is_some_and(|submodule| !submodule.url.is_empty());
            if !has_url {
                continue;
            }
            let rel = brix_store::os_path::to_path(gitlink.rel()).ok_or_else(|| {
                err!(
                    "submodule path '{}' is not valid UTF-8, which brix does not support on this platform",
                    gitlink
                )
            })?;
            let checkout = path.join(rel);
            // It becomes the submodule input's URL, which is text.
            let checkout = checkout.to_str().ok_or_else(|| {
                err!(
                    "path '{}' is not valid UTF-8, which brix does not support",
                    checkout.display()
                )
            })?;
            let mut attrs = Attrs::new();
            attrs.insert("type".into(), Attr::String("git".into()));
            attrs.insert("url".into(), Attr::String(checkout.to_owned()));
            attrs.insert("exportIgnore".into(), Attr::Bool(export_ignore));
            attrs.insert("submodules".into(), Attr::Bool(true));
            let submodule_input = Input::from_attrs(settings, attrs)?;
            let display = format!("«{}»", submodule_input.to_url_text(true)?);
            let fetched = get_accessor(ctx, &submodule_input, Some(display), false)
                .map_err(|e| e.trace(format!("while fetching the input '{submodule_input}'")))?;
            if fetched.input.rev()?.is_none() {
                is_dirty = true;
            }
            mounts.push((gitlink.clone(), fetched.accessor));
        }
    }
    // What an earlier fetch found is about the tree without its
    // submodules' checkouts.
    let known = if mounts.is_empty() { known } else { None };
    let known = |name: &str| known.and_then(|k| k.get(name)).cloned();
    let mut input = input.clone();
    if !is_dirty {
        if let Some(name) = workdir_ref(repo)? {
            input.attrs.insert("ref".into(), Attr::String(name));
        }
        // A repository without commits has the null revision.
        let rev = info.head_rev.unwrap_or(GitRev::NULL);
        input.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
        if count_revs && !maybe_get_bool_attr(&input.attrs, "shallow")?.unwrap_or(false) {
            let count = match known("revCount") {
                _ if rev.is_null() => Attr::Int(0),
                // `getRevCount`: a shallow clone has no full history.
                _ if repo.is_shallow() => {
                    return Err(err!(
                        "'{}' is a shallow Git repository, so 'revCount' is not available",
                        path.display()
                    ))
                }
                Some(count) => count,
                None => Attr::Int(rev_count(repo, &find_commit(repo, &rev)?)?),
            };
            input.attrs.insert("revCount".into(), count);
        }
        // `verifyCommit(input, repo)`: `HEAD`'s signature.
        super::git_verify::verify(&input, path)?;
    } else {
        // `RepoInfo::warnDirty`.
        let location = path.display();
        if !settings.allow_dirty {
            return Err(err!("Git tree '{}' has uncommitted changes", location));
        }
        if settings.warn_dirty {
            warn(&format!("Git tree '{location}' has uncommitted changes"));
        }
        if let Some(head) = info.head_rev {
            input.attrs.insert(
                "dirtyRev".into(),
                Attr::String(format!("{}-dirty", head.to_hex())),
            );
            input.attrs.insert(
                "dirtyShortRev".into(),
                Attr::String(format!("{}-dirty", head.short())),
            );
        }
        // `verifyCommit(input, nullptr)`: no commit to verify.
        super::git_verify::verify_dirty(&input)?;
    }
    let last_modified = match (info.head_rev, known("lastModified")) {
        (None, _) => Attr::Int(0),
        (Some(_), Some(time)) => time,
        (Some(head), None) => Attr::Int(commit_time(&find_commit(repo, &head)?)?),
    };
    input.attrs.insert("lastModified".into(), last_modified);

    let verified = match known("narHash") {
        Some(hash) => {
            input.attrs.insert("narHash".into(), hash);
            true
        }
        None => false,
    };
    // The tracked files (`AllowListSourceAccessor`), without what
    // `export-ignore` names (the index's attributes), with the submodules
    // mounted over their gitlinks.
    let files = WorkdirAccessor::new(path, &info.files, &info.submodules);
    let mut tree: Arc<dyn SourceAccessor> = match display {
        Some(display) => Arc::new(files.shown_as(display)),
        None => Arc::new(files),
    };
    if export_ignore {
        let attrs = Arc::new(GitAttributes::new(AttrSources::for_workdir(repo)?));
        tree = Arc::new(ExportIgnoreAccessor::new(
            tree,
            attrs,
            info.submodules.clone(),
        ));
    }
    if !mounts.is_empty() {
        let mut mounted = MountedAccessor::new(tree);
        for (gitlink, submodule) in mounts {
            mounted.mount(gitlink, submodule);
        }
        tree = Arc::new(mounted);
    }
    // `fixupAccessor`: a clean tree gets the input's fingerprint
    // (`git:<rev>`, mounted submodules or not), a dirty one none here (its
    // `;d=` fingerprint covers what is dirty, so its hash is its own).
    if !is_dirty {
        if let Some(fingerprint) = input.fingerprint()? {
            tree = with_fingerprint(tree, fingerprint, ctx.hashes);
        }
    }
    Ok(FetchedTree {
        accessor: tree,
        input,
        verified,
    })
}

/// The commit time (`lastModified`), in seconds since the epoch.
fn commit_time(commit: &gix::Commit<'_>) -> Result<u64> {
    let time = commit
        .time()
        .map_err(|e| gix_err("reading commit time", e))?;
    Ok(time.seconds.max(0) as u64)
}

/// `revCount`: the number of commits reachable from `commit`.
fn rev_count(repo: &gix::Repository, commit: &gix::Commit<'_>) -> Result<u64> {
    let count = repo
        .rev_walk([commit.id])
        .all()
        .map_err(|e| gix_err("walking history", e))?
        .count();
    Ok(count as u64)
}

/// Opens the bare clone at `dir`, creating it if needed. Like Nix's
/// `initRepoAtomically`, a new clone is set up in a temporary directory
/// and renamed into place, so that no process ever sees half of one. A
/// directory that is no repository (left by an older brix killed while
/// cloning) is set up anew. The caller holds the clone's lock
/// ([`lock_clone`]), so whatever another process left half done is stale.
fn open_or_init_bare(dir: &Path) -> Result<gix::Repository> {
    let io = |context: String| move |e| crate::Error::Io { context, source: e };
    if dir.exists() {
        // As Nix does before running `git fetch`: an interrupted fetch
        // leaves its lock on the shallow file behind.
        let shallow_lock = dir.join("shallow.lock");
        match std::fs::remove_file(&shallow_lock) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(format!("removing '{}'", shallow_lock.display()))(e)),
        }
        match gix::open(dir) {
            Ok(repo) => return Ok(repo),
            Err(gix::open::Error::NotARepository { .. }) => {
                std::fs::remove_dir_all(dir).map_err(|e| crate::Error::Io {
                    context: format!("removing the broken Git cache '{}'", dir.display()),
                    source: e,
                })?;
            }
            Err(e) => {
                return Err(gix_err(
                    &format!("opening Git cache '{}'", dir.display()),
                    e,
                ))
            }
        }
    }
    // Under the lock nobody else is making this clone: a temporary one
    // that exists was left by a process killed while making it.
    let mut tmp = dir.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::symlink_metadata(&tmp).is_ok() {
        std::fs::remove_dir_all(&tmp).map_err(io(format!("removing '{}'", tmp.display())))?;
    }
    gix::init_bare(&tmp)
        .map_err(|e| gix_err(&format!("creating Git repository '{}'", tmp.display()), e))?;
    std::fs::rename(&tmp, dir).map_err(io(format!(
        "moving temporary git repository from '{}' to '{}'",
        tmp.display(),
        dir.display()
    )))?;
    gix::open(dir).map_err(|e| gix_err(&format!("opening Git cache '{}'", dir.display()), e))
}

/// What `getAccessorFromCommit` fetches, in Nix's words (git prints the
/// refs it updates): every ref (`allRefs`), the revision, or the ref. The
/// revision goes to a ref of its own, in Nix's reserved namespace, so
/// that a later fetch of a nearby one can negotiate against it; `HEAD`
/// goes to `HEAD`, which git takes for `refs/heads/HEAD` (where
/// [`resolve_ref`] finds it, and whose age decides the next fetch, as in
/// Nix). The fetch is forced (`git fetch --force`).
fn fetch_refspec(all_refs: bool, rev: Option<GitRev>, ref_: &str) -> String {
    if all_refs {
        "refs/*:refs/*".to_owned()
    } else if let Some(rev) = rev {
        let rev = rev.to_hex();
        format!(
            "{rev}:refs/__nix_internal_fetchers_48ae34d6b380c5ec__/tip-{}",
            &rev[..2]
        )
    } else if ref_.starts_with("refs/") {
        format!("{ref_}:{ref_}")
    } else if ref_ == "HEAD" {
        "HEAD:HEAD".to_owned()
    } else {
        format!("refs/heads/{ref_}:refs/heads/{ref_}")
    }
}

/// `GitRepo::fetch` without `git` ([`Ctx::git`]; Nix's libgit2 fallback):
/// fetch `refspec` (and nothing else: no other branch, no tags) of `url`
/// into the clone with gix, forced as `git fetch --force` is, and with
/// `HEAD` fetched to where git puts it.
fn fetch_remote(
    http: &HttpClient,
    repo: &gix::Repository,
    url: &str,
    refspec: &str,
    shallow: bool,
) -> Result<()> {
    let interrupt = AtomicBool::new(false);
    let refspec = if refspec == "HEAD:HEAD" {
        "HEAD:refs/heads/HEAD"
    } else {
        refspec
    };
    let forced = format!("+{refspec}");
    let remote = repo
        .remote_at(url)
        .map_err(|e| gix_err("configuring remote", e))?
        .with_refspecs([forced.as_str()], gix::remote::Direction::Fetch)
        .map_err(|e| gix_err("configuring refspec", e))?
        .with_fetch_tags(gix::remote::fetch::Tags::None);
    connect(http, &remote, url)?
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|e| remote_err(&format!("preparing fetch from '{url}'"), e))?
        .with_shallow(if shallow {
            gix::remote::fetch::Shallow::DepthAtRemote(1.try_into().expect("1 is non-zero"))
        } else {
            gix::remote::fetch::Shallow::NoChange
        })
        .receive(gix::progress::Discard, &interrupt)
        .map_err(|e| remote_err(&format!("fetching '{url}'"), e))?;
    Ok(())
}

/// A connection for fetching from `remote` (whose URL is `url`) without
/// `git` ([`Ctx::git`]): over brix's HTTP client for `http` and `https`
/// (see [`super::git_http`]), so that gix trusts the certificates
/// downloads trust, and with gix's own transports for the rest (`file`,
/// `ssh`, `git`).
fn connect<'a, 'repo>(
    http: &HttpClient,
    remote: &'a gix::Remote<'repo>,
    url: &str,
) -> Result<gix::remote::Connection<'a, 'repo, Box<dyn Transport + Send>>> {
    let connect_err = |e| remote_err(&format!("connecting to '{url}'"), e);
    let (gix_url, version) = remote
        .sanitized_url_and_version(gix::remote::Direction::Fetch)
        .map_err(connect_err)?;
    if matches!(
        gix_url.scheme,
        gix::url::Scheme::Http | gix::url::Scheme::Https
    ) {
        let transport = gix::protocol::transport::client::http::connect_http(
            super::git_http::UreqHttp::new(http, url),
            gix_url,
            version,
            false,
        );
        return Ok(
            remote.to_connection_with_transport(Box::new(transport) as Box<dyn Transport + Send>)
        );
    }
    remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(connect_err)
}

/// `readHead` without `git` ([`Ctx::git`]): what a repository's `HEAD`
/// is, as `git ls-remote --symref` reports it ([`read_head_with_git`]):
/// the branch it points at (`refs/heads/main`), or the revision when it
/// is detached. `None` when there is no such `HEAD`.
fn read_head(http: &HttpClient, repo: &gix::Repository, url: &str) -> Result<Option<String>> {
    use gix::protocol::handshake::Ref;
    let remote = repo
        .remote_at(url)
        .map_err(|e| gix_err("configuring remote", e))?
        .with_refspecs(["HEAD"], gix::remote::Direction::Fetch)
        .map_err(|e| gix_err("configuring refspec", e))?;
    let prepare = connect(http, &remote, url)?
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|e| remote_err(&format!("listing the references of '{url}'"), e))?;
    // Dropping the preparation closes the connection without fetching.
    Ok(prepare.ref_map().remote_refs.iter().find_map(|r| match r {
        Ref::Symbolic {
            full_ref_name,
            target,
            ..
        } if full_ref_name == "HEAD" => Some(target.to_string()),
        Ref::Direct {
            full_ref_name,
            object,
        } if full_ref_name == "HEAD" => Some(object.to_string()),
        _ => None,
    }))
}

/// The remote `HEAD` the clone remembers (see [`store_cached_head`]): the
/// branch its own `HEAD` points at, if that branch exists in it.
fn cached_head(repo: &gix::Repository) -> Result<Option<String>> {
    let head = repo
        .head()
        .map_err(|e| gix_err("reading the cached HEAD", e))?;
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => Some(reference.name.as_bstr().to_string()),
        gix::head::Kind::Detached { target, .. } => Some(target.to_string()),
        gix::head::Kind::Unborn(_) => None,
    })
}

/// `time(nullptr)`: the time, in whole seconds since the epoch.
fn unix_now() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("invariant: the clock is past 1970");
    i64::try_from(now.as_secs()).expect("invariant: the time fits in 64 bits")
}

/// `maybeStat` and `isCacheFileWithinTtl`: whether `file` exists and was
/// modified (in whole seconds) less than `ttl` seconds before `now`; one
/// modified in the future is.
fn within_ttl(file: &Path, ttl: u64, now: i64) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(file) {
        Ok(meta) => Ok(meta.mtime().saturating_add_unsigned(ttl) > now),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(crate::Error::Io {
            context: format!("getting status of '{}'", file.display()),
            source: e,
        }),
    }
}

/// `setWriteTime(path, now, now)`: the access and modification times of
/// `path` (not of what it links to) set to `now`. A failure is the text
/// of Nix's `SysError`, colours and all.
fn set_write_time(path: &Path, now: i64) -> std::result::Result<(), String> {
    use rustix::fs::{utimensat, AtFlags, Timestamps, CWD};
    let time = rustix::fs::Timespec {
        tv_sec: now,
        tv_nsec: 0,
    };
    let times = Timestamps {
        last_access: time,
        last_modification: time,
    };
    utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW).map_err(|errno| {
        let e = std::io::Error::from_raw_os_error(errno.raw_os_error());
        format!(
            "changing modification time of \x1b[35;1m\"{}\"\x1b[0m (using `utimensat`): \x1b[35;1m{}\x1b[0m",
            path.display(),
            super::path::sys_error_text(&e)
        )
    })
}

/// `getDefaultRef` for a repository to fetch from: `readHeadCached` (the
/// remembered `HEAD` while it is younger than `tarball-ttl`, else the
/// remote's, else the remembered one anyway), or `master`. The remote's
/// `HEAD` is asked of `git ls-remote`, as Nix asks it, so that git says
/// why it cannot answer; without `git`, of gix, whose error brix gives as
/// a warning.
fn default_remote_ref(ctx: &Ctx<'_>, repo: &gix::Repository, url: &str) -> Result<String> {
    let Ctx {
        http,
        settings,
        warn,
        git,
        ..
    } = *ctx;
    let cached = cached_head(repo)?;
    if let Some(cached) = &cached {
        let head_file = repo.path().join("HEAD");
        if within_ttl(&head_file, settings.tarball_ttl, unix_now())? {
            return Ok(cached.clone());
        }
    }
    // `git ls-remote` failing only means there is no answer: git says why
    // on stderr.
    let remote = if git {
        read_head_with_git(url)?
    } else {
        read_head(http, repo, url).unwrap_or_else(|e| {
            warn(&e.to_string());
            None
        })
    };
    if let Some(head) = remote {
        return Ok(head);
    }
    if let Some(cached) = cached {
        warn(&format!(
            "could not get HEAD ref for repository '{url}'; using expired cached ref '{cached}'"
        ));
        return Ok(cached);
    }
    warn(&format!(
        "could not read HEAD ref from repo at '{url}', using 'master'"
    ));
    Ok("master".to_owned())
}

/// `ExecutablePath::load().findName("git")`, which decides whether Nix
/// fetches with `git` (see [`Ctx::git`]): whether a directory of `PATH`
/// (an empty entry being the current directory) holds a regular file
/// `git` that this process may execute.
fn git_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    // `parseAppend`: an empty `PATH` has no directories at all.
    if path.is_empty() {
        return false;
    }
    std::env::split_paths(&path).any(|dir| {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join("git");
        // `isExecutableFileAmbient`: a regular file (symbolic links
        // followed) that `access(X_OK)` allows.
        candidate.is_file() && rustix::fs::access(&candidate, rustix::fs::Access::EXEC_OK).is_ok()
    })
}

/// `git` with `args`, in the bare clone at `clone` if there is one (`-C
/// <clone> --git-dir .`), run as Nix's `runProgram` runs it: stdin and
/// stderr are brix's own, so that git asks for credentials and says what
/// fails as it does under Nix (or, in a fetch made ahead of time, held
/// back with the fetch's other messages: [`super::stderr`]); stdout is
/// collected.
fn run_git(clone: Option<&Path>, args: &[&str]) -> Result<std::process::Output> {
    let mut git = std::process::Command::new("git");
    if let Some(clone) = clone {
        git.arg("-C").arg(clone).args(["--git-dir", "."]);
    }
    let out = git
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stderr(super::stderr::for_program())
        .output()
        .map_err(|e| crate::Error::Io {
            context: "executing 'git'".into(),
            source: e,
        })?;
    super::stderr::pass_on(&out.stderr);
    Ok(out)
}

/// `readHead` as Nix runs it: `git ls-remote --symref <url>`, with git's
/// own messages passed on. `None` when git fails or names no `HEAD`.
fn read_head_with_git(url: &str) -> Result<Option<String>> {
    let out = run_git(None, &["ls-remote", "--symref", url])?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(out.stdout)
        .map_err(|_| err!("'git ls-remote --symref {}' printed no UTF-8", url))?;
    Ok(text.lines().next().and_then(ls_remote_head))
}

/// `git::parseLsRemoteLine` for the line about `HEAD`: `ref: <branch>`
/// for a symbolic one, the revision for a detached one.
fn ls_remote_head(line: &str) -> Option<String> {
    let (target, reference) = line.split_once('\t')?;
    if reference.trim_start_matches('\t') != "HEAD" {
        return None;
    }
    let target = match target.strip_prefix("ref:") {
        Some(branch) => branch.trim_start_matches(' '),
        None => target,
    };
    Some(target.to_owned())
}

/// `GitRepoImpl::fetch` as Nix runs it: `git -C <clone> --git-dir . fetch
/// --progress --force [--depth 1] -- <url> <refspec>`, with git's own
/// messages passed on; a failure is "Failed to fetch git repository
/// '<url>'".
fn fetch_with_git(clone: &Path, url: &str, refspec: &str, shallow: bool) -> Result<()> {
    let mut args = vec!["fetch", "--progress", "--force"];
    if shallow {
        args.extend(["--depth", "1"]);
    }
    args.extend(["--", url, refspec]);
    if !run_git(Some(clone), &args)?.status.success() {
        return Err(err!("Failed to fetch git repository '{}'", url));
    }
    Ok(())
}

/// `storeCachedHead`: remember the remote's `HEAD` branch as the clone's
/// (at `clone`) own `HEAD`, which also renews it for `tarball-ttl`. Nix
/// runs `git symbolic-ref`, which refuses a target outside `refs/` and
/// says so, and warns when it fails; without `git` brix edits the
/// reference with gix.
fn store_cached_head(
    git: bool,
    clone: &Path,
    repo: &gix::Repository,
    ref_: &str,
    url: &str,
    warn: &dyn Fn(&str),
) -> Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
    let stored = if git {
        let out = run_git(Some(clone), &["symbolic-ref", "--", "HEAD", ref_])?;
        // `ExecError`, which Nix lets through unless git exited.
        if out.status.code().is_none() {
            use std::os::unix::process::ExitStatusExt;
            return Err(match out.status.signal() {
                Some(signal) => err!("program 'git' was killed by signal {}", signal),
                None => err!("program 'git' died abnormally"),
            });
        }
        out.status.success()
    } else {
        ref_.starts_with("refs/")
            && match gix::refs::FullName::try_from(ref_) {
                Ok(target) => repo
                    .edit_reference(RefEdit {
                        change: Change::Update {
                            log: LogChange::default(),
                            expected: PreviousValue::Any,
                            new: gix::refs::Target::Symbolic(target),
                        },
                        name: "HEAD".try_into().expect("HEAD is a valid name"),
                        deref: false,
                    })
                    .is_ok(),
                Err(_) => false,
            }
    };
    if !stored {
        warn(&format!(
            "could not update cached head '{ref_}' for '{url}'"
        ));
    }
    Ok(())
}

fn oid(rev: &GitRev) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(rev.as_bytes())
}

fn find_commit<'r>(repo: &'r gix::Repository, rev: &GitRev) -> Result<gix::Commit<'r>> {
    repo.find_object(oid(rev))
        .map_err(|_| err!("Cannot find Git revision '{}' in repository", rev))?
        .try_into_commit()
        .map_err(|e| gix_err("reading commit", e))
}

/// `GitRepo::resolveRef`: `git_revparse_single(<ref>^{commit})`, which
/// takes the first of `<ref>`, `refs/<ref>`, `refs/tags/<ref>`,
/// `refs/heads/<ref>`, `refs/remotes/<ref>` and `refs/remotes/<ref>/HEAD`
/// that exists and leads to an object (`git_reference_dwim`): a tag the
/// fetch brought along (`git fetch` follows those pointing into what it
/// fetched) wins over a branch of its name, and a clone's `HEAD`, whose
/// branch it does not have, gives way to `refs/heads/HEAD` (where a fetch
/// of `HEAD` puts it).
fn resolve_ref(repo: &gix::Repository, ref_: &str) -> Result<GitRev> {
    let candidates = [
        ref_.to_owned(),
        format!("refs/{ref_}"),
        format!("refs/tags/{ref_}"),
        format!("refs/heads/{ref_}"),
        format!("refs/remotes/{ref_}"),
        format!("refs/remotes/{ref_}/HEAD"),
    ];
    for name in &candidates {
        // A name that is no valid reference name is passed over, as
        // libgit2 does.
        let Ok(full) = gix::refs::FullName::try_from(name.as_str()) else {
            continue;
        };
        let found = repo
            .try_find_reference(full.as_ref())
            .map_err(|e| gix_err(&format!("looking up '{name}'"), e))?;
        // gix looks a name up under the same prefixes; only the exact one
        // counts here.
        let Some(mut reference) = found.filter(|r| r.name() == full.as_ref()) else {
            continue;
        };
        // `git_reference_lookup_resolved`: a symbolic reference to a
        // reference that does not exist is passed over.
        if let gix::refs::TargetRef::Symbolic(target) = reference.target() {
            let resolves = repo
                .try_find_reference(target)
                .map_err(|e| gix_err(&format!("looking up '{target}'"), e))?
                .is_some_and(|t| t.name() == target);
            if !resolves {
                continue;
            }
        }
        let id = reference
            .peel_to_id()
            .map_err(|e| gix_err(&format!("resolving '{name}'"), e))?;
        return rev_from_oid(id.detach());
    }
    Err(err!("cannot resolve Git reference '{}'", ref_))
}

pub(super) fn rev_from_oid(id: gix::ObjectId) -> Result<GitRev> {
    let bytes: [u8; 20] = id
        .as_bytes()
        .try_into()
        .map_err(|_| err!("SHA-256 Git repositories are not supported"))?;
    Ok(GitRev::from_bytes(bytes))
}

/// `GitRepo::getWorkdirRef`: the branch `HEAD` points at, if any.
fn workdir_ref(repo: &gix::Repository) -> Result<Option<String>> {
    Ok(repo
        .head_name()
        .map_err(|e| gix_err("looking up HEAD", e))?
        .map(|n| n.as_bstr().to_string()))
}

/// `GitRepo::getAccessor(rev)`: the commit's tree as an in-memory tree,
/// its names and symlink targets the bytes Git has
/// (`git_tree_entry_name`, unchecked). A gitlink (submodule) is an empty
/// directory, as Nix's accessor shows it (submodules, when wanted, are
/// mounted over it). Also the tree's `.gitattributes` files by
/// directory, for `exportIgnore` (libgit2 reads any blob at such a path,
/// a symlink's included).
fn export_tree(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<(MemoryAccessor, super::git_attrs::AttributeFiles)> {
    let tree = commit
        .tree()
        .map_err(|e| gix_err("reading commit tree", e))?;
    let mut accessor = MemoryAccessor::from_root(MemoryNode::Directory(BTreeMap::new()));
    let mut attribute_files = HashMap::new();
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| gix_err("walking Git tree", e))?;
    for entry in entries {
        let path = super::git_workdir::tracked_path(entry.filepath.as_ref())?;
        use gix::objs::tree::EntryKind;
        match entry.mode.kind() {
            EntryKind::Tree => accessor.add_dir(&path)?,
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let data = repo
                    .find_object(entry.oid)
                    .map_err(|e| gix_err("reading blob", e))?
                    .detach()
                    .data;
                if let Some(dir) = super::git_attrs::commit_attr_file(path.rel()) {
                    attribute_files.insert(dir.to_vec(), data.clone());
                }
                accessor.add_file(&path, data, entry.mode.kind() == EntryKind::BlobExecutable)?;
            }
            EntryKind::Link => {
                let data = repo
                    .find_object(entry.oid)
                    .map_err(|e| gix_err("reading symlink", e))?
                    .detach()
                    .data;
                if let Some(dir) = super::git_attrs::commit_attr_file(path.rel()) {
                    attribute_files.insert(dir.to_vec(), data.clone());
                }
                accessor.add_symlink(&path, &data)?;
            }
            EntryKind::Commit => accessor.add_dir(&path)?,
        }
    }
    Ok((accessor, attribute_files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::tree_nar_hash;

    #[test]
    fn cache_path_is_keyed_by_url_hash() {
        let a = cache_path(Path::new("/c"), "https://x/y", false);
        let b = cache_path(Path::new("/c"), "https://x/y", true);
        assert!(a.starts_with("/c/gitv3"));
        assert_ne!(a, b);
        assert!(b.to_string_lossy().ends_with("-shallow"));
    }

    use brix_testutil::{git, GitRepo};

    /// A checkout with branches `main` (checked out) and `other` and a
    /// tag, and a bare clone of it next to it, which brix fetches like a
    /// remote.
    struct Repos {
        _repo: GitRepo,
        work: PathBuf,
        bare: PathBuf,
        cache: PathBuf,
    }

    impl Repos {
        fn new() -> Repos {
            let repo = GitRepo::new();
            repo.write("f", "main\n");
            repo.commit();
            repo.git(&["tag", "v1"]);
            repo.git(&["checkout", "-q", "-b", "other"]);
            repo.write("f", "other\n");
            repo.commit();
            repo.git(&["checkout", "-q", "main"]);
            let work = repo.path().to_owned();
            let bare = repo.scratch().join("bare.git");
            git(
                repo.scratch(),
                &["clone", "-q", "--bare", work.to_str().unwrap(), "bare.git"],
            );
            let cache = repo.scratch().join("cache");
            Repos {
                work,
                bare,
                cache,
                _repo: repo,
            }
        }

        fn rev(&self, name: &str) -> String {
            git(&self.work, &["rev-parse", name])
        }

        fn fetch(&self, url: &str, warnings: &RefCell<Vec<String>>) -> FetchedTree {
            let settings = FetchSettings::default();
            let input = Input::from_url_str(&settings, url, true).unwrap();
            fetch(
                &HttpClient::new(),
                &self.cache,
                &settings,
                &input,
                &|w: &str| warnings.borrow_mut().push(w.to_owned()),
            )
            .unwrap()
        }

        /// The refs of the clone `url` was fetched into.
        fn clone_refs(&self, url: &str) -> Vec<String> {
            let dir = cache_path(&self.cache, url, false);
            let refs = git(&dir, &["for-each-ref", "--format=%(refname)"]);
            refs.lines().map(str::to_owned).collect()
        }
    }

    use std::cell::RefCell;

    #[test]
    fn a_remote_without_ref_is_fetched_at_its_head_branch() {
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let url = format!("file://{}", repos.bare.display());
        let fetched = repos.fetch(&format!("git+{url}"), &warnings);
        assert_eq!(fetched.input.ref_().unwrap(), Some("refs/heads/main"));
        assert_eq!(
            fetched.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("main")
        );
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());
        // Only that branch was fetched, no other, and, as `git fetch`
        // follows them, the tags on it (Nix's clone has the same refs).
        assert_eq!(repos.clone_refs(&url), ["refs/heads/main", "refs/tags/v1"]);
    }

    #[test]
    fn a_remote_is_fetched_by_ref_and_by_rev() {
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let url = format!("file://{}", repos.bare.display());
        // By revision alone: the revision goes to a ref of its own (Nix's,
        // as git names it in what it prints), and the ref recorded is the
        // remote's HEAD branch. The tag on it comes along, as in Nix.
        let main = repos.rev("main");
        let by_rev = repos.fetch(&format!("git+{url}?rev={main}"), &warnings);
        assert_eq!(by_rev.input.ref_().unwrap(), Some("refs/heads/main"));
        assert_eq!(by_rev.input.rev().unwrap().unwrap().to_hex(), main);
        let tip = format!(
            "refs/__nix_internal_fetchers_48ae34d6b380c5ec__/tip-{}",
            &main[..2]
        );
        let tag = "refs/tags/v1".to_owned();
        assert_eq!(repos.clone_refs(&url), [tip.clone(), tag.clone()]);

        let other = repos.fetch(&format!("git+{url}?ref=other"), &warnings);
        assert_eq!(other.input.ref_().unwrap(), Some("other"));
        assert_eq!(
            other.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("other")
        );
        assert_eq!(
            repos.clone_refs(&url),
            [tip, "refs/heads/other".to_owned(), tag]
        );
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());
    }

    /// A ref fetched less than `tarball-ttl` ago is taken from the clone
    /// as it is (by any input naming it: here the second names it by
    /// itself); an older one is fetched again, into the same clone.
    #[test]
    fn a_second_fetch_updates_the_existing_clone() {
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let url = format!("git+file://{}", repos.bare.display());
        let first = repos.fetch(&url, &warnings);
        std::fs::write(repos.work.join("f"), "newer\n").unwrap();
        git(&repos.work, &["commit", "-q", "-am", "newer"]);
        git(
            &repos.work,
            &["push", "-q", repos.bare.to_str().unwrap(), "main"],
        );
        let again = repos.fetch(&format!("{url}?ref=main"), &warnings);
        assert_eq!(again.input.rev().unwrap(), first.input.rev().unwrap());
        let settings = FetchSettings {
            tarball_ttl: 0,
            ..FetchSettings::default()
        };
        let input = Input::from_url_str(&settings, &url, true).unwrap();
        let second = fetch(
            &HttpClient::new(),
            &repos.cache,
            &settings,
            &input,
            &|w: &str| warnings.borrow_mut().push(w.to_owned()),
        )
        .unwrap();
        assert_ne!(second.input.rev().unwrap(), first.input.rev().unwrap());
        assert_eq!(
            second.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("main")
        );
        assert_eq!(second.input.ref_().unwrap(), Some("refs/heads/main"));
        // The remote's HEAD was remembered for the second fetch.
        let dir = cache_path(
            &repos.cache,
            &format!("file://{}", repos.bare.display()),
            false,
        );
        assert_eq!(git(&dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    }

    /// When fetching a ref the clone has fails, Nix logs the error, warns
    /// and goes on with the clone's version.
    #[test]
    fn a_failed_fetch_of_a_ref_the_clone_has_is_got_past() {
        use std::sync::{Arc, Mutex};
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let remote = format!("file://{}", repos.bare.display());
        let url = format!("git+{remote}?ref=main");
        let first = repos.fetch(&url, &warnings);
        // The remote is gone: an empty directory is no repository.
        std::fs::remove_dir_all(&repos.bare).unwrap();
        std::fs::create_dir(&repos.bare).unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let logged = errors.clone();
        let settings = FetchSettings {
            tarball_ttl: 0,
            error: crate::Warn::new(move |e| logged.lock().unwrap().push(e.to_owned())),
            ..FetchSettings::default()
        };
        let input = Input::from_url_str(&settings, &url, true).unwrap();
        let again = fetch(
            &HttpClient::new(),
            &repos.cache,
            &settings,
            &input,
            &|w: &str| warnings.borrow_mut().push(w.to_owned()),
        )
        .unwrap();
        assert_eq!(again.input.rev().unwrap(), first.input.rev().unwrap());
        let failed = format!("Failed to fetch git repository '{remote}'");
        assert_eq!(*errors.lock().unwrap(), std::slice::from_ref(&failed));
        assert_eq!(
            *warnings.borrow(),
            [format!(
                "could not update local clone of Git repository '{remote}'; continuing with the most recent version"
            )]
        );
        // A ref the clone does not have cannot be got past.
        let other =
            Input::from_url_str(&settings, &format!("git+{remote}?ref=other"), true).unwrap();
        let Err(err) = fetch(
            &HttpClient::new(),
            &repos.cache,
            &settings,
            &other,
            &|_: &str| {},
        ) else {
            panic!("a ref the clone does not have is fetched");
        };
        assert_eq!(err.to_string(), failed);
    }

    /// `HEAD`, as a ref, is fetched to `refs/heads/HEAD`, where it is
    /// found, as in Nix.
    #[test]
    fn head_as_a_ref_is_fetched_to_a_branch_of_its_name() {
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let remote = format!("file://{}", repos.bare.display());
        let fetched = repos.fetch(&format!("git+{remote}?ref=HEAD"), &warnings);
        assert_eq!(fetched.input.ref_().unwrap(), Some("HEAD"));
        assert_eq!(
            fetched.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("main")
        );
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());
        let refs = repos.clone_refs(&remote);
        assert!(refs.contains(&"refs/heads/HEAD".to_owned()), "{refs:?}");
    }

    #[test]
    fn a_broken_clone_directory_is_set_up_again() {
        let repos = Repos::new();
        let url = format!("file://{}", repos.bare.display());
        let dir = cache_path(&repos.cache, &url, false);
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::write(dir.join("junk"), "left by a killed clone").unwrap();
        let warnings = RefCell::new(Vec::new());
        let fetched = repos.fetch(&format!("git+{url}"), &warnings);
        assert_eq!(
            fetched.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("main")
        );
        assert!(!dir.join("junk").exists());
    }

    /// Another process holding a clone's lock (`<clone>.lock`, as Nix's
    /// `PathLocks` names it) makes a fetch into the clone wait; a
    /// temporary clone a killed process left is set up anew under it.
    #[test]
    fn fetches_into_a_clone_wait_for_other_processes() {
        let repos = Repos::new();
        let url = format!("file://{}", repos.bare.display());
        let dir = cache_path(&repos.cache, &url, false);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        let mut tmp = dir.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::create_dir_all(Path::new(&tmp).join("objects")).unwrap();
        let mut lock = dir.as_os_str().to_owned();
        lock.push(".lock");
        let ready = repos.cache.join("ready");
        let release = repos.cache.join("release");
        let mut holder =
            super::super::lock::tests::spawn_holder(Path::new(&lock), &ready, &release);
        let fetcher = std::thread::spawn({
            let cache = repos.cache.clone();
            let url = url.clone();
            move || {
                let settings = FetchSettings::default();
                let input = Input::from_url_str(&settings, &format!("git+{url}"), true).unwrap();
                fetch(&HttpClient::new(), &cache, &settings, &input, &|_: &str| {}).map(|f| f.input)
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!fetcher.is_finished(), "the fetch waits for the lock");
        assert!(!dir.exists());
        std::fs::write(&release, "").unwrap();
        assert!(holder.wait().unwrap().success());
        let input = fetcher.join().unwrap().unwrap();
        assert_eq!(input.rev().unwrap().unwrap().to_hex(), repos.rev("main"));
        assert!(!Path::new(&tmp).exists());
    }

    /// [`fetch`] as when no `git` is on `PATH` ([`Ctx::git`]): with gix.
    fn fetch_with_gix(
        http: &HttpClient,
        cache_dir: &Path,
        settings: &FetchSettings,
        input: &Input,
    ) -> Result<FetchedTree> {
        let ctx = Ctx {
            http,
            cache_dir,
            settings,
            warn: &|_: &str| {},
            hashes: None,
            git: false,
        };
        get_accessor(&ctx, input, None, true)
    }

    /// With `git` on `PATH`, Git over HTTPS is git's, as in Nix: it trusts
    /// what git trusts, not the CA file brix's downloads trust
    /// (`ssl-cert-file`), which Nix does not hand to git either; the test
    /// CA is not among what git trusts. Without `git` it goes through
    /// brix's HTTP client, so it trusts that CA file and nothing else.
    #[test]
    fn https_remotes_trust_git_or_the_ca_file() {
        use super::super::test_https::{fixture, git_http_backend, serve};
        assert!(git_on_path(), "the tests need `git` on PATH");
        let repos = Repos::new();
        let root = repos.bare.parent().unwrap().to_path_buf();
        let base = serve(move |request| git_http_backend(&root, request));
        let url = format!("git+{base}/bare.git");
        let settings = FetchSettings::default();
        let input = Input::from_url_str(&settings, &url, true).unwrap();
        let http = HttpClient::with_ca_file(Some(&fixture("ca.pem")));
        let Err(err) = fetch(&http, &repos.cache, &settings, &input, &|_: &str| {}) else {
            panic!("git trusts the test CA");
        };
        assert_eq!(
            err.to_string(),
            format!("Failed to fetch git repository '{base}/bare.git'")
        );

        let fetch_with = |ca: &Path| {
            let http = HttpClient::with_ca_file(Some(ca));
            fetch_with_gix(&http, &repos.cache, &settings, &input)
        };
        let fetched = fetch_with(&fixture("ca.pem")).unwrap();
        assert_eq!(
            fetched.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("main")
        );
        assert_eq!(fetched.input.ref_().unwrap(), Some("refs/heads/main"));
        // Another branch needs another fetch.
        let other = Input::from_url_str(&settings, &format!("{url}?ref=other"), true).unwrap();
        let http = HttpClient::with_ca_file(Some(&fixture("ca.pem")));
        let fetched = fetch_with_gix(&http, &repos.cache, &settings, &other).unwrap();
        assert_eq!(
            fetched.input.rev().unwrap().unwrap().to_hex(),
            repos.rev("other")
        );

        let third = Input::from_url_str(&settings, &format!("{url}?ref=v1"), true).unwrap();
        for ca in [
            fixture("other-ca.pem"),
            PathBuf::from("/nonexistent/ca.pem"),
        ] {
            let http = HttpClient::with_ca_file(Some(&ca));
            let Err(err) = fetch_with_gix(&http, &repos.cache, &settings, &third) else {
                panic!("{}: an untrusted server is refused", ca.display());
            };
            let text = err.to_string();
            let why = if ca.exists() {
                "invalid peer certificate: UnknownIssuer"
            } else {
                "Problem with the SSL CA cert (path? access rights?) (77) error adding trust anchors from file: /nonexistent/ca.pem"
            };
            assert!(text.contains(why), "{text}");
        }
    }

    /// A local checkout fetched by revision records the branch checked out.
    #[test]
    fn a_local_checkout_by_rev_records_its_branch() {
        let repos = Repos::new();
        let warnings = RefCell::new(Vec::new());
        let other = repos.rev("other");
        let fetched = repos.fetch(
            &format!("git+file://{}?rev={other}", repos.work.display()),
            &warnings,
        );
        assert_eq!(fetched.input.ref_().unwrap(), Some("refs/heads/main"));
        assert_eq!(fetched.input.rev().unwrap().unwrap().to_hex(), other);
    }

    /// A commit's names and symlink targets are bytes, as Git has them,
    /// and `export-ignore` matches them as bytes; the error for an ignored
    /// path names it with its bytes, as Nix's does.
    #[test]
    fn paths_in_a_commit_are_bytes() {
        let repos = Repos::new();
        brix_testutil::write_bytes(&repos.work, b"caf\xe9", "1");
        brix_testutil::write_bytes(&repos.work, b"d\xe9/f", "2");
        brix_testutil::symlink_bytes(&repos.work, b"l\xff", b"t\xe9");
        std::fs::write(repos.work.join(".gitattributes"), "caf* export-ignore\n").unwrap();
        git(&repos.work, &["add", "-A"]);
        git(&repos.work, &["commit", "-q", "-m", "latin-1"]);
        let rev = repos.rev("main");
        let settings = FetchSettings::default();
        for query in ["", "&exportIgnore=1"] {
            let url = format!("git+file://{}?rev={rev}{query}", repos.work.display());
            let input = Input::from_url_str(&settings, &url, true).unwrap();
            let fetched = fetch(
                &HttpClient::new(),
                &repos.cache,
                &settings,
                &input,
                &|_: &str| {},
            )
            .unwrap();
            let tree = &fetched.accessor;
            let names = tree.read_dir(&CanonPath::root()).unwrap();
            assert!(names.contains_key(BStr::new(b"d\xe9")), "{query}");
            assert_eq!(tree.read_file(&p(b"/d\xe9/f")).unwrap(), b"2");
            assert_eq!(tree.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9");
            if query.is_empty() {
                assert_eq!(tree.read_file(&p(b"/caf\xe9")).unwrap(), b"1");
            } else {
                assert!(!names.contains_key(BStr::new(b"caf\xe9")));
                assert_eq!(
                    tree.read_file(&p(b"/caf\xe9")).unwrap_err().message_bytes(),
                    b"'/caf\xe9' does not exist because it was fetched with exportIgnore enabled"
                );
            }
        }
    }

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    /// A repository `sub` and a checkout `repo` (on `main`) that has it
    /// as the submodule `deps/sub`, by the relative URL `../sub`.
    struct WithSubmodule {
        dir: tempfile::TempDir,
        repo: PathBuf,
        cache: PathBuf,
    }

    impl WithSubmodule {
        fn new() -> WithSubmodule {
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("sub");
            let repo = dir.path().join("repo");
            for d in [&sub, &repo] {
                std::fs::create_dir(d).unwrap();
                git(d, &["init", "-q", "-b", "main"]);
            }
            std::fs::write(sub.join("s.txt"), "in the submodule\n").unwrap();
            git(&sub, &["add", "-A"]);
            git(&sub, &["commit", "-q", "-m", "s"]);
            std::fs::write(repo.join("top.txt"), "top\n").unwrap();
            git(
                &repo,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "--quiet",
                    "add",
                    "../sub",
                    "deps/sub",
                ],
            );
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "top"]);
            let cache = dir.path().join("cache");
            WithSubmodule { dir, repo, cache }
        }

        fn rev(&self) -> String {
            git(&self.repo, &["rev-parse", "HEAD"])
        }

        fn fetch(
            &self,
            settings: &FetchSettings,
            url: &str,
            warnings: &RefCell<Vec<String>>,
        ) -> Result<FetchedTree> {
            let input = Input::from_url_str(settings, url, true).unwrap();
            fetch(
                &HttpClient::new(),
                &self.cache,
                settings,
                &input,
                &|w: &str| warnings.borrow_mut().push(w.to_owned()),
            )
        }
    }

    /// `submodules = true` mounts each submodule of a commit, fetched at
    /// its gitlink's revision, and the tree's fingerprint says so (`;s`).
    #[test]
    fn the_submodules_of_a_commit_are_mounted_when_asked_for() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let rev = t.rev();
        let url = format!("git+file://{}?rev={rev}", t.repo.display());

        let plain = t.fetch(&settings, &url, &warnings).unwrap();
        assert!(plain.accessor.read_dir(&p("/deps/sub")).unwrap().is_empty());
        assert_eq!(
            plain.accessor.fingerprint(),
            Some(format!("git:{rev}").as_str())
        );

        let with = t
            .fetch(&settings, &format!("{url}&submodules=1"), &warnings)
            .unwrap();
        assert_eq!(
            with.accessor.read_file(&p("/deps/sub/s.txt")).unwrap(),
            b"in the submodule\n"
        );
        assert_eq!(with.accessor.read_file(&p("/top.txt")).unwrap(), b"top\n");
        assert_eq!(
            with.accessor.fingerprint(),
            Some(format!("git:{rev};s").as_str())
        );
        assert_eq!(with.input.rev().unwrap().unwrap().to_hex(), rev);
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());
    }

    /// A bare clone is fetched like a remote: a relative submodule URL
    /// resolves against its `file://` URL as libgit2 does it, losing the
    /// `/` after `file://`, which Nix then refuses; an absolute one works.
    #[test]
    fn the_submodules_of_a_remote_resolve_against_its_url() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let bare = t.dir.path().join("bare.git");
        git(
            t.dir.path(),
            &[
                "clone",
                "-q",
                "--bare",
                t.repo.to_str().unwrap(),
                "bare.git",
            ],
        );
        let rev = t.rev();
        let url = format!("git+file://{}?rev={rev}&submodules=1", bare.display());
        let err = t.fetch(&settings, &url, &warnings).unwrap_err().to_string();
        assert!(err.contains("has unexpected authority"), "{err}");

        let sub = t.dir.path().join("sub");
        git(
            &t.repo,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.deps/sub.url",
                sub.to_str().unwrap(),
            ],
        );
        git(&t.repo, &["commit", "-q", "-am", "absolute"]);
        git(&t.repo, &["push", "-q", bare.to_str().unwrap(), "main"]);
        let rev = t.rev();
        let url = format!("git+file://{}?rev={rev}&submodules=1", bare.display());
        let fetched = t.fetch(&settings, &url, &warnings).unwrap();
        assert_eq!(
            fetched.accessor.read_file(&p("/deps/sub/s.txt")).unwrap(),
            b"in the submodule\n"
        );
    }

    /// A working tree's submodules are their checkouts, read in place; a
    /// change in one makes the whole tree dirty, the submodule warned
    /// about first.
    #[test]
    fn a_working_trees_submodules_are_read_in_place() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let url = format!("git+file://{}?submodules=1", t.repo.display());

        let plain = t
            .fetch(
                &settings,
                &format!("git+file://{}", t.repo.display()),
                &warnings,
            )
            .unwrap();
        assert!(plain.accessor.read_dir(&p("/deps/sub")).unwrap().is_empty());

        let clean = t.fetch(&settings, &url, &warnings).unwrap();
        assert_eq!(
            clean.accessor.read_file(&p("/deps/sub/s.txt")).unwrap(),
            b"in the submodule\n"
        );
        assert_eq!(clean.input.rev().unwrap().unwrap().to_hex(), t.rev());
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());

        let checkout = t.repo.join("deps/sub");
        std::fs::write(checkout.join("s.txt"), "changed\n").unwrap();
        let dirty = t.fetch(&settings, &url, &warnings).unwrap();
        assert_eq!(
            dirty.accessor.read_file(&p("/deps/sub/s.txt")).unwrap(),
            b"changed\n"
        );
        assert_eq!(dirty.input.rev().unwrap(), None);
        assert_eq!(
            dirty.input.attrs.get("dirtyRev"),
            Some(&Attr::String(format!("{}-dirty", t.rev())))
        );
        assert_eq!(
            *warnings.borrow(),
            [
                format!("Git tree '{}' has uncommitted changes", checkout.display()),
                format!("Git tree '{}' has uncommitted changes", t.repo.display()),
            ]
        );
    }

    #[test]
    fn export_ignore_and_submodules_are_refused_together() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let by_rev = format!("rev={}&submodules=1&exportIgnore=1", t.rev());
        for query in ["submodules=1&exportIgnore=1", by_rev.as_str()] {
            let url = format!("git+file://{}?{query}", t.repo.display());
            assert_eq!(
                t.fetch(&settings, &url, &warnings).unwrap_err().to_string(),
                "exportIgnore and submodules are not supported together yet"
            );
        }
    }

    /// `exportIgnore` hides what `export-ignore` names, in a commit and
    /// in a working tree, and says so when it is read.
    #[test]
    fn export_ignore_hides_what_the_attributes_name() {
        let repo = GitRepo::new();
        repo.write(".gitattributes", "ignored export-ignore\n");
        repo.write("ignored", "i\n");
        repo.write("kept", "k\n");
        let rev = repo.commit();
        let settings = FetchSettings::default();
        let cache = repo.scratch().join("cache");
        let base = format!("git+file://{}", repo.path().display());
        // A clean working tree has the fingerprint of its `HEAD` too.
        for url in [
            format!("{base}?rev={rev}&exportIgnore=1"),
            format!("{base}?exportIgnore=1"),
        ] {
            let fingerprint = Some(format!("git:{rev};e"));
            let input = Input::from_url_str(&settings, &url, true).unwrap();
            let fetched =
                fetch(&HttpClient::new(), &cache, &settings, &input, &|_: &str| {}).unwrap();
            let root: Vec<_> = fetched
                .accessor
                .read_dir(&CanonPath::root())
                .unwrap()
                .into_keys()
                .collect();
            assert_eq!(root, [".gitattributes", "kept"], "{url}");
            assert_eq!(
                fetched
                    .accessor
                    .read_file(&p("/ignored"))
                    .unwrap_err()
                    .to_string(),
                "'/ignored' does not exist because it was fetched with exportIgnore enabled"
            );
            assert_eq!(fetched.accessor.fingerprint(), fingerprint.as_deref());
        }
    }

    /// A repository whose `git archive` differs from its tree:
    /// `export-subst` and `export-ignore`.
    fn archive_repo() -> (GitRepo, String) {
        let repo = GitRepo::new();
        repo.write(
            ".gitattributes",
            "subst.txt export-subst\nignored export-ignore\n",
        );
        repo.write("subst.txt", "Commit: $Format:%H$\n");
        repo.write("ignored", "i\n");
        let rev = repo.commit();
        (repo, rev)
    }

    /// With `nix-219-compat` a commit is exported by `git archive`, which
    /// Nix announces and fingerprints as `;e;legacy`.
    #[test]
    fn nix_219_compat_exports_with_git_archive() {
        let (repo, rev) = archive_repo();
        let settings = FetchSettings {
            nix219_compat: true,
            ..FetchSettings::default()
        };
        let warnings = RefCell::new(Vec::new());
        let url = format!("git+file://{}?rev={rev}", repo.path().display());
        let input = Input::from_url_str(&settings, &url, true).unwrap();
        let fetched = fetch(
            &HttpClient::new(),
            &repo.scratch().join("cache"),
            &settings,
            &input,
            &|w: &str| warnings.borrow_mut().push(w.to_owned()),
        )
        .unwrap();
        assert_eq!(
            fetched.accessor.read_file(&p("/subst.txt")).unwrap(),
            format!("Commit: {rev}\n").into_bytes()
        );
        assert!(!fetched.accessor.path_exists(&p("/ignored")).unwrap());
        assert_eq!(
            fetched.accessor.fingerprint(),
            Some(format!("git:{rev};e;legacy").as_str())
        );
        assert_eq!(
            *warnings.borrow(),
            [format!(
                "Using Nix 2.19 semantics to export Git repository 'git+file://{}?ref=refs/heads/main&rev={rev}'.",
                repo.path().display()
            )]
        );
        // A working tree is read in place either way.
        assert!(!bypasses_fetch_cache(
            &settings,
            &Input::from_url_str(
                &settings,
                &format!("git+file://{}", repo.path().display()),
                true
            )
            .unwrap()
        )
        .unwrap());
        assert!(bypasses_fetch_cache(&settings, &input).unwrap());
        assert!(!bypasses_fetch_cache(&FetchSettings::default(), &input).unwrap());
    }

    /// A locked `narHash` made by the other export is accepted, with
    /// Nix's warning when it was Nix 2.19's.
    #[test]
    fn a_nar_hash_made_by_the_other_export_is_accepted() {
        let (repo, rev) = archive_repo();
        let compat = FetchSettings {
            nix219_compat: true,
            ..FetchSettings::default()
        };
        let current = FetchSettings::default();
        let cache = repo.scratch().join("cache");
        let url = format!("git+file://{}?rev={rev}", repo.path().display());
        let fetch_with = |settings: &FetchSettings, nar_hash: Option<&NarHash>| {
            let mut input = Input::from_url_str(settings, &url, true).unwrap();
            if let Some(hash) = nar_hash {
                input
                    .attrs
                    .insert("narHash".into(), Attr::String(hash.to_sri()));
            }
            let warnings = RefCell::new(Vec::new());
            let fetched = fetch(&HttpClient::new(), &cache, settings, &input, &|w: &str| {
                warnings.borrow_mut().push(w.to_owned())
            })
            .unwrap();
            (fetched, warnings.into_inner())
        };
        let legacy_hash = tree_nar_hash(fetch_with(&compat, None).0.accessor.as_ref()).unwrap();
        let current_hash = tree_nar_hash(fetch_with(&current, None).0.accessor.as_ref()).unwrap();
        assert_ne!(legacy_hash, current_hash);

        let (fetched, warnings) = fetch_with(&current, Some(&legacy_hash));
        assert!(fetched.verified);
        assert_eq!(fetched.input.nar_hash().unwrap(), Some(legacy_hash));
        assert_eq!(
            fetched.accessor.fingerprint(),
            Some(format!("git:{rev};e;legacy").as_str())
        );
        assert_eq!(
            warnings,
            [format!(
                "Git input 'git+file://{}?ref=refs/heads/main&rev={rev}' specifies a NAR hash '{}' that was created by Nix < 2.20.\nNix >= 2.20 does not apply Git filters, `export-ignore` and `export-subst` by default, which changes the NAR hash.\nPlease update the NAR hash to '{}'.",
                repo.path().display(),
                legacy_hash.to_sri(),
                current_hash.to_sri()
            )]
        );

        // The other way round only the export is announced.
        let (fetched, warnings) = fetch_with(&compat, Some(&current_hash));
        assert_eq!(fetched.input.nar_hash().unwrap(), Some(current_hash));
        assert_eq!(
            fetched.accessor.read_file(&p("/subst.txt")).unwrap(),
            b"Commit: $Format:%H$\n"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        // Neither: the tree of the export asked for, unverified.
        let wrong = NarHash::from_bytes([0; 32]);
        let (fetched, _) = fetch_with(&current, Some(&wrong));
        assert!(!fetched.verified);
        assert_eq!(
            tree_nar_hash(fetched.accessor.as_ref()).unwrap(),
            current_hash
        );
    }

    #[test]
    fn ls_remote_lines_name_head() {
        assert_eq!(
            ls_remote_head("ref: refs/heads/main\tHEAD").as_deref(),
            Some("refs/heads/main")
        );
        let rev = "a".repeat(40);
        assert_eq!(
            ls_remote_head(&format!("{rev}\tHEAD")).as_deref(),
            Some(rev.as_str())
        );
        assert_eq!(ls_remote_head(&format!("{rev}\trefs/heads/main")), None);
        assert_eq!(ls_remote_head("garbage"), None);
    }

    /// The NAR hash of a tree with a fingerprint is remembered under it, as
    /// Nix's `fetchToStore` remembers it: a working tree's (`git:<rev>`)
    /// is the same with its submodules mounted and without, and the same
    /// as its commit's without them, so the first one hashed gives the
    /// others its hash. A commit with its submodules mounted is apart
    /// (`;s`).
    #[test]
    fn nar_hashes_are_remembered_by_fingerprint() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let rev = t.rev();
        let workdir = format!("git+file://{}", t.repo.display());
        let hash_with = |cache: &str, url: &str| {
            let fetcher = super::super::NativeFetcher::new()
                .unwrap()
                .with_cache_dir(t.dir.path().join(cache));
            let input = Input::from_url_str(&settings, url, true).unwrap();
            crate::fetch::fetch_input(&fetcher, &settings, &input)
                .unwrap()
                .input
                .nar_hash()
                .unwrap()
                .unwrap()
        };
        let own_plain = hash_with("fresh-1", &workdir);
        let own_with = hash_with("fresh-2", &format!("{workdir}?submodules=1"));
        assert_ne!(own_plain, own_with);

        let with = hash_with("shared", &format!("{workdir}?submodules=1"));
        assert_eq!(with, own_with);
        assert_eq!(hash_with("shared", &workdir), own_with);
        assert_eq!(
            hash_with("shared", &format!("{workdir}?rev={rev}")),
            own_with
        );
        assert_eq!(
            hash_with("shared", &format!("{workdir}?rev={rev}&submodules=1")),
            own_with
        );

        // The other way round, the commit with its submodules has its own.
        assert_eq!(hash_with("other", &workdir), own_plain);
        assert_eq!(
            hash_with("other", &format!("{workdir}?submodules=1")),
            own_plain
        );
        assert_eq!(
            hash_with("other", &format!("{workdir}?rev={rev}&submodules=1")),
            own_with
        );
    }

    /// A submodule whose checkout is missing is an empty directory, which
    /// Nix takes for a repository to fetch from and has `git` read: each
    /// of the two tries of the parent warns twice about its `HEAD`, and
    /// the fetch fails.
    #[test]
    fn missing_submodule_checkouts_are_fetched_like_nix() {
        let t = WithSubmodule::new();
        git(
            t.dir.path(),
            &["clone", "-q", t.repo.to_str().unwrap(), "clone"],
        );
        let clone = t.dir.path().join("clone");
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let url = format!("git+file://{}?submodules=1", clone.display());
        let err = t.fetch(&settings, &url, &warnings).unwrap_err();
        let sub = format!("file://{}/deps/sub", clone.display());
        assert_eq!(
            err.to_string(),
            format!("Failed to fetch git repository '{sub}'")
        );
        assert_eq!(
            err.traces(),
            [format!("while fetching the input 'git+{sub}?submodules=1'")]
        );
        let warning = format!("could not read HEAD ref from repo at '{sub}', using 'master'");
        assert_eq!(*warnings.borrow(), vec![warning; 4]);
    }

    /// A relative submodule URL is resolved against the checkout's remote
    /// after `url.<base>.insteadOf` rewrote it.
    #[test]
    fn submodule_urls_follow_insteadof() {
        let t = WithSubmodule::new();
        git(
            &t.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/org/repo.git",
            ],
        );
        git(
            &t.repo,
            &[
                "config",
                &format!("url.{}/.insteadOf", t.dir.path().display()),
                "https://example.invalid/org/",
            ],
        );
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let url = format!(
            "git+file://{}?rev={}&submodules=1",
            t.repo.display(),
            t.rev()
        );
        let fetched = t.fetch(&settings, &url, &warnings).unwrap();
        assert_eq!(
            fetched.accessor.read_file(&p("/deps/sub/s.txt")).unwrap(),
            b"in the submodule\n"
        );
    }

    /// The files of a submodule's working tree are shown as Nix shows
    /// them: the submodule's input, then their place on disk.
    #[test]
    fn submodule_working_trees_are_shown_as_their_input() {
        let t = WithSubmodule::new();
        let settings = FetchSettings::default();
        let warnings = RefCell::new(Vec::new());
        let url = format!("git+file://{}?submodules=1", t.repo.display());
        let fetched = t.fetch(&settings, &url, &warnings).unwrap();
        let checkout = t.repo.join("deps/sub");
        assert_eq!(
            fetched.accessor.mounted_show_path(&p("/deps/sub/s.txt")),
            Some(BString::from(format!(
                "«git+file://{}?submodules=1»{}/s.txt",
                checkout.display(),
                checkout.display()
            )))
        );
        assert_eq!(fetched.accessor.mounted_show_path(&p("/top.txt")), None);
    }
}
