//! `brix build`, `brix run` and `brix env shell`: evaluate, hand the
//! derivations to the daemon, and act on the outputs
//! (`src/nix/build.cc`, `run.cc`, `env.cc`).

use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::installable::DerivedPath;
use brix::store::BuildMode;
use brix::{Session, StorePath};
use brix_store::SourceAccessor as _;
use bstr::BString;
use serde_json::{json, Map, Value};

use crate::commands::realise::{
    build_requests, exec_program, is_in_store, output_paths_of, print_missing, requests_for,
    require_local_store, resolve_app, Built, MixEnvironment,
};
use crate::commands::{print_stdout, Ctx};
use crate::output::json_text;
use crate::{CliError, Result};

/// `IndirectRootStore::addPermRoot`, which makes every out-link
/// (`createOutLinks`, `eval --drv-link`, `flake prefetch --out-link`,
/// `bundle`): `gc_root`, made canonical, becomes a symlink to `path`
/// registered as a GC root, unless that would replace something other
/// than a symlink into the store. Paths are bytes, as in Nix: an
/// out-link in a directory whose name is not UTF-8 is made and named as
/// it is.
pub fn add_perm_root(session: &Session, path: &StorePath, gc_root: impl AsRef<[u8]>) -> Result<()> {
    let link = perm_root_link(session, gc_root)?;
    make_perm_root(session, path, &link)
}

/// `what`, then `path` in double quotes (`PathFmt`), then `after`: a
/// message or an error's context naming a path, its bytes as they are.
fn quoting(what: &str, path: &[u8], after: &str) -> Vec<u8> {
    [what.as_bytes(), b"\"", path, b"\"", after.as_bytes()].concat()
}

/// The checks `addPermRoot` makes before it touches the store: the link
/// `gc_root` names, made canonical, if it is outside the store and would
/// replace nothing but a symlink into the store. [`make_perm_root`]
/// makes it.
pub fn perm_root_link(session: &Session, gc_root: impl AsRef<[u8]>) -> Result<BString> {
    let store_dir = session.config().store_dir();
    let gc_root = brix_store::CanonPath::new(gc_root.as_ref())
        .map_err(brix::Error::from)?
        .into_bytes();
    if is_in_store(store_dir, &gc_root) {
        return Err(CliError::msg(quoting(
            "creating a garbage collector root (",
            &gc_root,
            ") in the Nix store is forbidden (are you running nix-build inside the store?)",
        )));
    }
    let link = Path::new(OsStr::from_bytes(&gc_root));
    // `pathExists` (an `lstat`), then whether it is a link into the store.
    match std::fs::symlink_metadata(link) {
        Ok(meta) => {
            let into_store = meta.file_type().is_symlink()
                && is_in_store(
                    store_dir,
                    std::fs::read_link(link)
                        .map_err(|e| {
                            brix::Error::io(quoting("reading symbolic link ", &gc_root, ""), e)
                        })?
                        .as_os_str()
                        .as_bytes(),
                );
            if !into_store {
                return Err(CliError::msg(quoting(
                    "cannot create symlink ",
                    &gc_root,
                    "; already exists",
                )));
            }
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(e) => {
            return Err(brix::Error::io(quoting("getting status of ", &gc_root, ""), e).into())
        }
    }
    Ok(gc_root.into())
}

/// The rest of `addPermRoot`, for a `link` [`perm_root_link`] accepted:
/// the symlink to `path`, registered with the store as a GC root.
pub fn make_perm_root(session: &Session, path: &StorePath, link: impl AsRef<[u8]>) -> Result<()> {
    let store_dir = session.config().store_dir();
    let link = Path::new(OsStr::from_bytes(link.as_ref()));
    make_symlink(link, &store_dir.display(path))?;
    session.store_add_indirect_root(link)?;
    Ok(())
}

/// `IndirectRootStore::makeSymlink`: the link's directory is created, and
/// a new link under a temporary name is renamed over the old one, so that
/// the link changes atomically.
fn make_symlink(link: &Path, target: &str) -> Result<()> {
    let link_bytes = link.as_os_str().as_bytes();
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            brix::Error::io(
                quoting("creating directory ", dir.as_os_str().as_bytes(), ""),
                e,
            )
        })?;
    }
    // Nix adds `rand()` to the process ID; a counter is as unique.
    static LINKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = LINKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut temp = link.as_os_str().to_owned();
    temp.push(format!(".tmp-{}-{n}", std::process::id()));
    std::os::unix::fs::symlink(target, &temp).map_err(|e| {
        brix::Error::io(
            quoting(
                "creating symlink ",
                link_bytes,
                &format!(" -> \"{target}\""),
            ),
            e,
        )
    })?;
    std::fs::rename(&temp, link)
        .map_err(|e| brix::Error::io(quoting("renaming over ", link_bytes, ""), e))?;
    Ok(())
}

/// `MixOutLinkByDefault`: the out-link prefix, `result` unless the last
/// of `--out-link` and `--no-link` says otherwise.
pub fn out_link<'c>(ctx: &'c Ctx<'_>) -> Option<&'c str> {
    match ctx.inv.last_of(&["out-link", "no-link"]) {
        Some(flag) if flag.name == "no-link" => None,
        Some(flag) => Some(&flag.values[0]),
        None => Some("result"),
    }
}

/// `createOutLinks`: `result`, `result-1`, `result-dev`, …, made only in a
/// store on this machine (`createOutLinksMaybe`'s `LocalFSStore`). Bytes,
/// as Nix's paths are: `out_link` is in the current directory unless it
/// says otherwise, whose name need not be UTF-8. (An output that was
/// built has a valid store path name, ASCII, by the time an out-link is
/// made: Nix refuses every other name before, see
/// `brix-cli/tests/output_names.rs`.)
pub(crate) fn create_out_links(
    session: &Session,
    out_link: impl AsRef<[u8]>,
    built: &[Built],
) -> Result<()> {
    if !session.store_is_local() {
        return Ok(());
    }
    for (i, b) in built.iter().enumerate() {
        for (output, path) in &b.outputs {
            let mut link = out_link.as_ref().to_vec();
            if i > 0 {
                link.extend_from_slice(format!("-{i}").as_bytes());
            }
            if !output.is_empty() && output != "out" {
                link.push(b'-');
                link.extend_from_slice(output);
            }
            add_perm_root(session, path, &link)?;
        }
    }
    Ok(())
}

/// `builtPathsWithResultToJSON` of one result: `BuiltPath::toJSON` (a
/// store path is its own string; a derivation's built outputs are an
/// object, `outputs` only when there are some, as nlohmann creates it on
/// first use) with the build's times. An output name that is not UTF-8
/// fails as nlohmann's dump does.
fn built_path_json(store_dir: &brix::StoreDir, b: &Built) -> Result<Value> {
    let mut j = Map::new();
    match &b.request {
        // Nix's `j["startTime"]` on a string would throw; a path that is
        // already there has no times to add.
        DerivedPath::Opaque(p) => return Ok(json!(store_dir.display(p))),
        DerivedPath::Built { drv_path, .. } => {
            j.insert("drvPath".into(), json!(store_dir.display(drv_path)));
            if !b.outputs.is_empty() {
                let outs = b
                    .outputs
                    .iter()
                    .map(|(n, p)| Ok((json_text(n)?.to_owned(), json!(store_dir.display(p)))))
                    .collect::<Result<Map<String, Value>>>()?;
                j.insert("outputs".into(), Value::Object(outs));
            }
        }
    }
    if let Some(r) = &b.result {
        if r.start_time != 0 {
            j.insert("startTime".into(), json!(r.start_time));
        }
        if r.stop_time != 0 {
            j.insert("stopTime".into(), json!(r.stop_time));
        }
        if let Some(us) = r.cpu_user_us {
            j.insert("cpuUser".into(), json!(us as f64 / 1_000_000.0));
        }
        if let Some(us) = r.cpu_system_us {
            j.insert("cpuSystem".into(), json!(us as f64 / 1_000_000.0));
        }
    }
    Ok(Value::Object(j))
}

/// `nix build`.
pub fn build(ctx: &Ctx<'_>) -> Result<()> {
    if ctx.inv.has("profile") {
        return Err(CliError::msg(
            "'--profile' is not supported: brix does not manage profiles",
        ));
    }
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-build"])?;
    let installables = ctx.installables(&resolver)?;
    let requests = requests_for(&mut resolver, &installables)?;
    let store_dir = session.config().store_dir().clone();
    if ctx.inv.has("dry-run") {
        let paths: Vec<DerivedPath> = requests.iter().map(|(_, r)| r.clone()).collect();
        print_missing(&session, &paths)?;
        if ctx.inv.has("json") {
            let mut arr = Vec::new();
            for p in &paths {
                arr.push(match p {
                    DerivedPath::Opaque(p) => json!(store_dir.display(p)),
                    DerivedPath::Built { drv_path, outputs } => {
                        // `DerivedPath::Built::toJSON` asks the daemon for the
                        // output map (`queryPartialDerivationOutputMap`),
                        // which fails for a derivation it does not have.
                        if session.derivation(drv_path).is_none()
                            && !session.store_is_valid_path(drv_path)?
                        {
                            return Err(CliError::msg(format!(
                                "path '{}' is not valid",
                                store_dir.display(drv_path)
                            )));
                        }
                        let outs = output_paths_of(&session, drv_path, outputs)?
                            .into_iter()
                            .map(|(n, p)| {
                                Ok((json_text(&n)?.to_owned(), json!(store_dir.display(&p))))
                            })
                            .collect::<Result<Map<String, Value>>>()?;
                        json!({ "drvPath": store_dir.display(drv_path), "outputs": outs })
                    }
                });
            }
            ctx.print_json(&Value::Array(arr))?;
        }
        return Ok(());
    }
    let mode = if ctx.inv.has("repair") {
        BuildMode::Repair
    } else if ctx.inv.has("rebuild") {
        BuildMode::Check
    } else {
        BuildMode::Normal
    };
    let built = build_requests(ctx, &session, &requests, mode)?;
    if ctx.inv.has("json") {
        let arr = built
            .iter()
            .map(|b| built_path_json(&store_dir, b))
            .collect::<Result<Vec<Value>>>()?;
        ctx.print_json_compact(&Value::Array(arr))?;
    }
    if let Some(link) = out_link(ctx) {
        create_out_links(&session, ctx.abs_path(link), &built)?;
    }
    if ctx.inv.has("print-out-paths") {
        let mut out = String::new();
        for b in &built {
            for (_, p) in &b.outputs {
                out.push_str(&store_dir.display(p));
                out.push('\n');
            }
        }
        print_stdout(&out)?;
    }
    Ok(())
}

/// `nix run`.
pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let environment = MixEnvironment::from_flags(ctx.inv)?;
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-run"])?;
    let installable = ctx.installable(&resolver)?;
    let app = resolver.app(&installable)?;
    resolve_app(ctx, &session, &app)?;
    let env = environment.environ()?;
    // `execProgramInStore` without a lookup path: argv[0] is the program.
    require_local_store(&session)?;
    let program = OsStr::new(&app.program);
    Err(exec_program(program, program, ctx.inv.arg("args"), &env))
}

/// `nix shell` (`nix env shell`): the installables built, their `bin`
/// directories (and those of the packages they propagate) put in front of
/// `PATH`, and the command run.
pub fn shell(ctx: &Ctx<'_>) -> Result<()> {
    let environment = MixEnvironment::from_flags(ctx.inv)?;
    // `CmdShell::command`: `$SHELL` as brix was started, else `bash`.
    let command: Vec<OsString> = match ctx.inv.flag("command") {
        Some([]) => return Err(CliError::usage("--command requires at least one argument")),
        Some(words) => words.iter().map(OsString::from).collect(),
        None => vec![std::env::var_os("SHELL").unwrap_or_else(|| "bash".into())],
    };
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-build"])?;
    let installables = ctx.installables(&resolver)?;
    let requests = requests_for(&mut resolver, &installables)?;
    let built = build_requests(ctx, &session, &requests, BuildMode::Normal)?;
    let mut env = environment.environ()?;
    let store_dir = session.config().store_dir().clone();
    let fs = brix_store::FsAccessor::new("/");
    let fs_error = |e: brix_store::AccessorError| CliError::from(brix::Error::from(e));
    let canon = |path: String| brix_store::CanonPath::new(&path).map_err(brix::Error::from);
    let mut todo: VecDeque<StorePath> = built
        .iter()
        .flat_map(|b| b.outputs.iter().map(|(_, p)| p.clone()))
        .collect();
    let mut done: BTreeSet<StorePath> = BTreeSet::new();
    let mut additions: Vec<OsString> = Vec::new();
    while let Some(path) = todo.pop_front() {
        if !done.insert(path.clone()) {
            continue;
        }
        let root = store_dir.display(&path);
        // `resolveSymlinks`: where `bin` leads, followed component by
        // component; a part that does not exist is kept as it is.
        let bin = fs
            .resolve_symlinks(&canon(format!("{root}/bin"))?, true)
            .map_err(fs_error)?;
        // Bytes, as the link targets on the way are.
        if !is_in_store(&store_dir, bin.abs()) {
            return Err(CliError::msg(
                [&b"path '"[..], bin.as_bytes(), b"' is not in the Nix store"].concat(),
            ));
        }
        additions.push(OsStr::from_bytes(bin.as_bytes()).to_owned());
        let propagated = fs
            .resolve_symlinks(
                &canon(format!("{root}/nix-support/propagated-user-env-packages"))?,
                true,
            )
            .map_err(fs_error)?;
        let regular = fs
            .maybe_lstat(&propagated)
            .map_err(fs_error)?
            .is_some_and(|st| st.file_type == brix_store::FileType::Regular);
        if regular {
            let text = fs.read_file(&propagated).map_err(fs_error)?;
            // `tokenizeString` and `parseStorePath`, which fails on a word
            // that is not a store path (with its bytes in the message).
            for word in text
                .split(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                .filter(|w| !w.is_empty())
            {
                todo.push_back(store_dir.parse(word).map_err(brix::Error::from)?);
            }
        }
    }
    // `ExecutablePath::load()` of the environment just set: its `PATH`,
    // with an empty entry meaning `.`, after the additions.
    let mut path = additions;
    if let Some(old) = env.get(OsStr::new("PATH")).filter(|p| !p.is_empty()) {
        path.extend(old.as_bytes().split(|&b| b == b':').map(|dir| {
            if dir.is_empty() {
                OsString::from(".")
            } else {
                OsStr::from_bytes(dir).to_owned()
            }
        }));
    }
    env.set("PATH", path.join(OsStr::new(":")));
    // `execProgramInStore` with the lookup path: `execvp`, which finds the
    // command in the `PATH` just set.
    require_local_store(&session)?;
    Err(exec_program(&command[0], &command[0], &command[1..], &env))
}
