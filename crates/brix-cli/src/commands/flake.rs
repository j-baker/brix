//! `brix flake …` (`src/nix/flake.cc`).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::installable::DerivedPath;
use crate::output::flake_metadata_json;
use brix::schemas;

use crate::schemas::{CheckEvent, InventoryOptions};
use brix::{LockFlags, Session};
use brix_flake::{Edge, FetchSettings, NodeId};
use serde_json::{json, Value};

use crate::commands::realise::{fs_path, realise, request, Requests};
use crate::commands::{print_stdout, Ctx, LVL_INFO, LVL_NOTICE};
use crate::{CliError, Result};

const ANSI_BOLD: &str = "\u{1b}[1m";
const ANSI_NORMAL: &str = "\u{1b}[0m";
const ANSI_GREEN: &str = "\u{1b}[32;1m";
const ANSI_WARNING: &str = "\u{1b}[35;1m";
const ANSI_ITALIC: &str = "\u{1b}[3m";
const TREE_CONN: &str = "├───";
const TREE_LAST: &str = "└───";
const TREE_LINE: &str = "│   ";
const TREE_NULL: &str = "    ";

/// `nix flake metadata` (and the deprecated `nix flake info`).
pub fn metadata(ctx: &Ctx<'_>, deprecated_alias: bool) -> Result<()> {
    if deprecated_alias {
        eprintln!("warning: 'nix flake info' is a deprecated alias for 'nix flake metadata'");
    }
    let session = ctx.session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let flags = ctx.lock_flags(&session)?;
    // `requireLockable = false`: the root's own reference is shown as written.
    // Locked only, not called, as in Nix.
    let locked = &session.lock_flake(&flake_ref, &flags, false)?;
    let f = &locked.flake;
    // "Hack to show the store path if available": Nix shows the flake's
    // store path when the store says it is valid. With lazy trees (which
    // Determinate's nix.conf turns on, and which brix's evaluation
    // follows) every flake, one in the store included, is mounted at a
    // virtual store path that is never valid, so there is no `Path:`
    // line and no `path` field. The store is asked all the same, so one
    // that cannot be reached is an error; `dummy://` has nothing to ask.
    if session.has_store() {
        session.store_connect()?;
    }
    // `Hash::to_string(Base16, false)`.
    let fingerprint = session
        .flake_fingerprint(locked)?
        .map(|h| h.to_string(brix_store::HashFormat::Base16, false));
    if ctx.inv.has("json") {
        let mut json = flake_metadata_json(locked)?;
        let obj = json.as_object_mut().expect("metadata is an object");
        if let Some(fp) = &fingerprint {
            obj.insert("fingerprint".into(), json!(fp));
        }
        return ctx.print_json(&json);
    }
    let mut out = String::new();
    let label = |s: &str| format!("{ANSI_BOLD}{s}{ANSI_NORMAL}");
    out.push_str(&format!("{}  {}\n", label("Resolved URL:"), f.resolved_ref));
    if f.locked_ref
        .input
        .is_locked(&FetchSettings::default())
        .map_err(brix::Error::Flake)?
    {
        out.push_str(&format!("{}    {}\n", label("Locked URL:"), f.locked_ref));
    }
    if let Some(d) = &f.metadata.description {
        out.push_str(&format!("{}   {d}\n", label("Description:")));
    }
    if let Some(rev) = f.locked_ref.input.rev().map_err(brix::Error::Flake)? {
        out.push_str(&format!("{}      {}\n", label("Revision:"), rev.to_hex()));
    }
    if let Some(brix_flake::Attr::String(dirty)) = f.locked_ref.input.attrs.get("dirtyRev") {
        out.push_str(&format!("{}      {dirty}\n", label("Revision:")));
    }
    if let Some(n) = f.locked_ref.input.rev_count().map_err(brix::Error::Flake)? {
        out.push_str(&format!("{}     {n}\n", label("Revisions:")));
    }
    if let Some(t) = f
        .locked_ref
        .input
        .last_modified()
        .map_err(brix::Error::Flake)?
    {
        out.push_str(&format!(
            "{} {}\n",
            label("Last modified:"),
            local_date_time(t)
        ));
    }
    if let Some(fp) = &fingerprint {
        out.push_str(&format!("{}   {fp}\n", label("Fingerprint:")));
    }
    let root = locked.lock_file.node(locked.lock_file.root);
    if !root.inputs.is_empty() {
        out.push_str(&format!("{}\n", label("Inputs:")));
        let mut visited = vec![locked.lock_file.root];
        print_inputs(locked, locked.lock_file.root, "", &mut out, &mut visited);
    }
    print_stdout(&out)
}

/// `std::put_time(std::localtime(&t), "%F %T")`: the flake's own
/// `lastModified` as `nix flake metadata` shows it, in the local time zone
/// (`TZ`, else the system's). A time the C library cannot represent
/// (Nix's `localtime` returns null there, which `put_time` dereferences)
/// is shown in UTC.
fn local_date_time(t: u64) -> String {
    extern "C" {
        fn tzset();
    }
    let Ok(time) = libc::time_t::try_from(t) else {
        return brix_flake::lockfile::format_date_time(t);
    };
    // SAFETY: `tm` is plain data that `localtime_r` fills in; `tzset` only
    // reads the environment, as `localtime` does in Nix.
    let tm = unsafe {
        tzset();
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return brix_flake::lockfile::format_date_time(t);
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// The input tree, the way `nix flake metadata` draws it.
fn print_inputs(
    locked: &brix::LockedFlake<brix::TreeHandle>,
    node: NodeId,
    prefix: &str,
    out: &mut String,
    visited: &mut Vec<NodeId>,
) {
    let inputs = &locked.lock_file.node(node).inputs;
    let n = inputs.len();
    for (i, (name, edge)) in inputs.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { TREE_LAST } else { TREE_CONN };
        match edge {
            Edge::Node(id) => {
                let child = locked.lock_file.node(*id);
                let locked_ref = child
                    .locked
                    .as_ref()
                    .map(|l| l.locked_ref.to_url_string(true).unwrap_or_default())
                    .unwrap_or_default();
                let when = child
                    .locked
                    .as_ref()
                    .and_then(|l| l.locked_ref.input.last_modified().ok().flatten())
                    .map(|t| format!(" ({})", brix_flake::lockfile::format_date_time(t)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "{prefix}{branch}{ANSI_BOLD}{name}{ANSI_NORMAL}: {locked_ref}{when}\n"
                ));
                if !visited.contains(id) {
                    visited.push(*id);
                    let next = format!("{prefix}{}", if last { TREE_NULL } else { TREE_LINE });
                    print_inputs(locked, *id, &next, out, visited);
                }
            }
            Edge::Follows(path) => {
                out.push_str(&format!(
                    "{prefix}{branch}{ANSI_BOLD}{name}{ANSI_NORMAL} follows input '{}'\n",
                    brix_flake::lockfile::print_input_attr_path(path)
                ));
            }
        }
    }
}

/// `nix flake show`.
pub fn show(ctx: &Ctx<'_>) -> Result<()> {
    let json = ctx.inv.has("json");
    let options = InventoryOptions {
        show_legacy: ctx.inv.has("legacy"),
        show_all_systems: ctx.inv.has("all-systems"),
        show_output_paths: ctx.inv.has("output-paths"),
        show_drv_paths: ctx.inv.has("drv-paths"),
        show_drv_names: ctx.inv.has("drv-names") || json,
    };
    if options.show_output_paths && !json {
        return Err(CliError::usage(
            "The '--output-paths' flag requires '--json'.",
        ));
    }
    if options.show_drv_paths && !json {
        return Err(CliError::usage("The '--drv-paths' flag requires '--json'."));
    }
    let session = ctx.session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let flake = session.flake_with_flags(&flake_ref, &ctx.lock_flags(&session)?, false)?;
    let defaults = ctx.default_schemas(&session)?;
    let root = schemas::schema_root(&session, &flake, defaults.as_ref())?;
    if json {
        let inv = crate::schemas::inventory_json(&session, &root, &options, &ctx.system())?;
        return ctx.print_json(&json!({ "version": 2, "inventory": inv }));
    }
    print_stdout(&format!(
        "{ANSI_BOLD}{}{ANSI_NORMAL}\n",
        flake.locked().flake.locked_ref
    ))?;
    let inventory = root.get_attr("inventory")?;
    let outputs = root.get_attr("outputs")?.renamed(Vec::new());
    let mut entries = schemas::for_each_output(&inventory)?;
    // Text uses the same alphabetical order as the JSON object, while
    // checks retain the schema's allowIFD order.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let n = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        let name = &entry.name;
        let last = i + 1 == n;
        let next_prefix = if last { TREE_NULL } else { TREE_LINE };
        let header = format!(
            "{ANSI_GREEN}{}{ANSI_NORMAL}{ANSI_BOLD}{name}{ANSI_NORMAL}",
            if last { TREE_LAST } else { TREE_CONN }
        );
        if let Some(output) = &entry.output {
            render_inventory(
                &output.renamed(vec![name.clone()]),
                &outputs,
                &options,
                &ctx.system(),
                &header,
                next_prefix,
            )?;
        } else {
            print_stdout(&format!(
                "{header}{ANSI_WARNING} unknown flake output{ANSI_NORMAL}\n"
            ))?;
        }
    }
    Ok(())
}

/// Print a node before evaluating its children, flushing each line even
/// when stdout is a pipe. A later evaluation failure keeps earlier output.
fn render_inventory(
    node: &brix::Cursor<'_>,
    outputs: &brix::Cursor<'_>,
    options: &InventoryOptions,
    local_system: &str,
    header: &str,
    next_prefix: &str,
) -> Result<()> {
    let mut obj = serde_json::Map::new();
    let mut printed = false;
    let result = (|| -> Result<()> {
        let children = crate::schemas::inventory_node(
            node.session(),
            node,
            outputs,
            options,
            local_system,
            &mut obj,
        )?;
        print_stdout(&render(&obj, header))?;
        printed = true;
        if let Some(children) = children {
            let names = children.get_attrs()?;
            for (i, name) in names.iter().enumerate() {
                let last = i + 1 == names.len();
                let header = format!(
                    "{ANSI_GREEN}{next_prefix}{}{ANSI_NORMAL}{ANSI_BOLD}{name}{ANSI_NORMAL}",
                    if last { TREE_LAST } else { TREE_CONN }
                );
                let next = format!("{next_prefix}{}", if last { TREE_NULL } else { TREE_LINE });
                let mut path = node.attr_path().to_vec();
                path.push(name.clone());
                let child = children.get_attr(name)?.renamed(path);
                render_inventory(&child, outputs, options, local_system, &header, &next)?;
            }
        }
        Ok(())
    })();
    match result {
        // Like the JSON walk, tolerate evaluation failures below
        // legacyPackages, preserving any node information already read.
        Err(CliError::Brix(brix::Error::Eval { .. }))
            if node.attr_path().len() > 1
                && node
                    .attr_path()
                    .first()
                    .is_some_and(|a| a == "legacyPackages") =>
        {
            if !printed {
                print_stdout(&render(&obj, header))?;
            }
            Ok(())
        }
        other => other,
    }
}

/// The text tree of `nix flake show`.
fn render(j: &serde_json::Map<String, Value>, header_prefix: &str) -> String {
    let mut s = header_prefix.to_owned();
    if let Some(what) = j.get("what").and_then(Value::as_str) {
        s.push_str(&format!(": {what}"));
    }
    if let Some(name) = j
        .get("derivation")
        .and_then(|d| d.get("name"))
        .and_then(Value::as_str)
    {
        s.push_str(&format!("{ANSI_ITALIC} [{name}]{ANSI_NORMAL}"));
    }
    if j.get("filtered").and_then(Value::as_bool) == Some(true) {
        s.push_str(&format!(
            " {ANSI_WARNING}omitted{ANSI_NORMAL} (use '--all-systems' to show)"
        ));
    }
    if j.get("isLegacy").and_then(Value::as_bool) == Some(true) {
        s.push_str(&format!(
            " {ANSI_WARNING}omitted{ANSI_NORMAL} (use '--legacy' to show)"
        ));
    }
    s.push('\n');
    s
}

/// `PathFmt`: a path in double quotes, its bytes as they are
/// (`path.string()`, nothing escaped). Nix's file-system errors
/// (`readFile`, `writeFile`, `createDirs`, `createSymlink`) and `flake
/// init`'s notices name paths this way.
fn quote_path(p: &Path) -> Vec<u8> {
    [&b"\""[..], p.as_os_str().as_bytes(), b"\""].concat()
}

/// `nix flake lock`: add missing entries, never change existing ones.
/// Unlocked inputs are asked about afresh, `--offline` or not.
pub fn lock(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.refetching_session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let mut flags = ctx.lock_flags(&session)?;
    flags.write_lock_file = true;
    // Nix's default, which `--no-update-lock-file` turns off (`lock_flags`
    // starts from the pure flags).
    flags.update_lock_file = !ctx.inv.has("no-update-lock-file");
    flags.fail_on_unlocked = true;
    // Locking may fetch unlocked inputs to pin them.
    flags.allow_unlocked = true;
    // Locking writes the lock file, as `lockFlake` does; the flake is not
    // called.
    session.lock_flake(&flake_ref, &flags, false)?;
    Ok(())
}

/// `nix flake update [inputs…]`. Unlocked inputs are asked about afresh,
/// `--offline` or not.
pub fn update(ctx: &Ctx<'_>) -> Result<()> {
    // The positionals are input paths, never the flake (that is
    // `--flake`), and Nix reads them while it parses the command line:
    // before it looks for the flake, and after every flag, wherever it
    // stands, so `--update-input` (deprecated) is read first and an input
    // it names is one a positional repeats.
    let mut input_updates = ctx.update_input_flags()?;
    for input in ctx.inv.arg("inputs") {
        // Nix warns about a bad input path and rethrows the error as a
        // plain `Error` (`throw e` of a caught `Error &`), so even the
        // zero-length one comes without the usage hint.
        let invalid = |e: CliError| {
            eprintln!(
                "warning: Invalid flake input '{input}'. To update a specific flake, use 'nix flake update --flake {input}' instead."
            );
            e
        };
        let path = brix_flake::lockfile::parse_input_attr_path(input)
            .map_err(|e| invalid(brix::Error::Flake(e).into()))?;
        if path.is_empty() {
            return Err(invalid(CliError::msg(
                "input path to be updated cannot be zero-length; it would refer to the flake itself, not an input",
            )));
        }
        if input_updates.contains(&path) {
            eprintln!(
                "warning: Input '{}' was specified multiple times. You may have done this by accident.",
                brix_flake::lockfile::print_input_attr_path(&path)
            );
        }
        input_updates.insert(path);
    }
    let session = ctx.refetching_session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let mut flags = ctx.lock_flags_with(&session, Some(input_updates))?;
    flags.recreate_lock_file = flags.input_updates.is_empty();
    flags.write_lock_file = true;
    flags.update_lock_file = true;
    flags.allow_unlocked = true;
    // Locking writes the lock file, as `lockFlake` does; the flake is not
    // called.
    session.lock_flake(&flake_ref, &flags, false)?;
    Ok(())
}

/// `nix flake check`.
pub fn check(ctx: &Ctx<'_>) -> Result<()> {
    let build = !ctx.inv.has("no-build");
    let mut config = ctx.config()?;
    if !build {
        config.eval.allow_import_from_derivation = false;
    }
    let session = Session::new(config)?;
    let flake_ref = ctx.flake_ref(&session)?;
    let flake = session.flake_with_flags(&flake_ref, &ctx.lock_flags(&session)?, false)?;
    let defaults = ctx.default_schemas(&session)?;
    let root = schemas::schema_root(&session, &flake, defaults.as_ref())?;
    let system = ctx.system();
    let keep_going = ctx.settings.get_bool("keep-going", false)?;
    let report = crate::schemas::check(
        &root,
        if ctx.inv.has("all-systems") {
            None
        } else {
            Some(&system)
        },
        build,
        ctx.inv.has("build-all"),
        keep_going,
        |event| match event {
            CheckEvent::Passed {
                attr_path,
                build_skipped,
            } => brix::log::stderr(&format!(
                "✅ {ANSI_BOLD}{attr_path}{ANSI_NORMAL}{}",
                if build_skipped {
                    format!("{ANSI_ITALIC}\u{1b}[2m (build skipped){ANSI_NORMAL}")
                } else {
                    String::new()
                }
            )),
            CheckEvent::Failed { attr_path, error } => {
                brix::log::stderr(&format!("❌ \u{1b}[31;1m{attr_path}{ANSI_NORMAL}"));
                eprintln!("{error}");
            }
        },
    )?;
    let mut has_errors = report.has_errors;
    if report.aborted {
        // Nix threw out of `checkFlake` at that failure: no summary
        // warnings, no builds, exit 1.
        return Err(CliError::Exit(1));
    }
    if !report.unchecked_outputs.is_empty() {
        eprintln!(
            "warning: The following flake outputs are unchecked: {}.",
            report
                .unchecked_outputs
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if build && !report.to_build.is_empty() && !has_errors {
        let paths: Vec<DerivedPath> = report
            .to_build
            .keys()
            .map(|p| DerivedPath::all_outputs(p.clone()))
            .collect();
        // Only what the daemon would actually build gets reported as
        // such; the rest was built before.
        let missing = session.store_query_missing(&paths)?;
        let to_build: BTreeSet<&brix::StorePath> = missing.will_build.iter().collect();
        for (drv, attrs) in &report.to_build {
            if !to_build.contains(drv) {
                for a in attrs {
                    brix::log::stderr(&format!("✅ {ANSI_BOLD}{a}{ANSI_NORMAL}{ANSI_ITALIC}\u{1b}[2m (previously built){ANSI_NORMAL}"));
                }
            }
        }
        let requests: Requests = report
            .to_build
            .iter()
            .filter(|(drv, _)| to_build.contains(drv))
            .map(|(drv, attrs)| request(attrs.join(", "), DerivedPath::all_outputs(drv.clone())))
            .collect();
        if !requests.is_empty() {
            match realise(&session, &requests, brix::store::BuildMode::Normal) {
                Ok(built) => {
                    for b in built {
                        // The attribute paths joined above, as bytes.
                        for a in bstr::ByteSlice::split_str(b.installable_text.as_slice(), ", ") {
                            let line = [
                                "✅ ".as_bytes(),
                                ANSI_BOLD.as_bytes(),
                                a,
                                ANSI_NORMAL.as_bytes(),
                            ]
                            .concat();
                            brix::log::stderr_bytes(&line);
                        }
                    }
                }
                Err(CliError::Exit(_)) => has_errors = true,
                Err(e) => return Err(e),
            }
        }
    }
    if !report.omitted_systems.is_empty() {
        eprintln!(
            "warning: The check omitted these incompatible systems: {}\nUse '--all-systems' to check all.",
            report
                .omitted_systems
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if has_errors {
        return Err(CliError::Exit(1));
    }
    Ok(())
}

/// `nix flake prefetch`.
pub fn prefetch(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let p = session.prefetch(&flake_ref, ctx.settings.get_bool("use-registries", true)?)?;
    let store_dir = session.config().store_dir().clone();
    let hash = p.nar_hash.to_string(brix_store::HashFormat::Sri, true);
    if ctx.inv.has("json") {
        let json = json!({
            "storePath": store_dir.display(&p.store_path),
            "hash": hash,
            "original": brix_flake::attrs::attrs_to_json(&p.resolved_ref.to_attrs()),
            "locked": brix_flake::attrs::attrs_to_json(&p.locked_ref.to_attrs()),
        });
        ctx.print_json(&json)?;
    } else {
        eprintln!(
            "Downloaded '{}' to '{}' (hash '{}').",
            p.locked_ref,
            store_dir.display(&p.store_path),
            hash
        );
    }
    if let Some(link) = ctx.inv.flag_value("out-link") {
        // A `LocalFSStore` only.
        if !session.store_is_local() {
            return Err(CliError::msg(
                "'--out-link' is not supported for this Nix store",
            ));
        }
        session.store_ensure_path(&p.store_path)?;
        crate::commands::build::add_perm_root(&session, &p.store_path, ctx.abs_path(link))?;
    }
    Ok(())
}

/// `nix flake prefetch-inputs`: fetch every locked input.
pub fn prefetch_inputs(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    // Locked only, not called, as in Nix.
    let locked = &session.lock_flake(&flake_ref, &ctx.lock_flags(&session)?, false)?;
    let mut inputs = Vec::new();
    let mut done: BTreeSet<NodeId> = BTreeSet::new();
    let mut todo = vec![locked.lock_file.root];
    while let Some(id) = todo.pop() {
        if !done.insert(id) {
            continue;
        }
        let node = locked.lock_file.node(id);
        if let Some(l) = &node.locked {
            // Build-time inputs are skipped, with what they bring.
            if l.build_time {
                continue;
            }
            // Relative path inputs are part of their parent's tree.
            if l.locked_ref.input.is_relative().is_none() {
                inputs.push(&l.locked_ref);
            }
        }
        for edge in node.inputs.values() {
            if let Edge::Node(child) = edge {
                todo.push(*child);
            }
        }
    }
    // All at once, as Nix's thread pool does; then each in turn.
    let to_fetch: Vec<brix_flake::Input> = inputs.iter().map(|r| r.input.clone()).collect();
    session.prefetch_inputs(&to_fetch);
    let mut failed = 0;
    for locked_ref in inputs {
        // The activity Nix starts for each fetch, which its logger shows
        // at `info`.
        if ctx.verbosity() >= LVL_INFO {
            brix::log::stderr(&format!("fetching '{locked_ref}'..."));
        }
        if let Err(e) = session.fetch_input(&locked_ref.input) {
            brix::log::stderr(&e.render(session.config().eval.show_trace));
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(CliError::Exit(1));
    }
    Ok(())
}

/// `nix flake init` / `nix flake new`.
pub fn init(ctx: &Ctx<'_>, new: bool) -> Result<()> {
    // Bytes, as the current directory's name is.
    let dest = if new {
        fs_path(
            &ctx.abs_path(
                ctx.inv
                    .arg_one("dest-dir")
                    .expect("the parser requires dest-dir"),
            ),
        )
        .to_path_buf()
    } else {
        fs_path(&ctx.cwd).to_path_buf()
    };
    let template = ctx
        .inv
        .flag_value("template")
        .unwrap_or("https://flakehub.com/f/DeterminateSystems/flake-templates/0.1");
    let session = ctx.session()?;
    let mut resolver = ctx.resolver_with(
        &session,
        // `CmdFlakeInitCommon::lockFlags`.
        LockFlags {
            write_lock_file: false,
            ..LockFlags::default()
        },
        &["nix-template"],
    )?;
    let installables = resolver.parse(&[template.to_owned()])?;
    let cursor = resolver.cursor(&installables[0])?;
    // `coerceToPath`: the template is read where it is, not copied to the
    // store first, and shown as Nix shows a path.
    let (accessor, from, template_path) = cursor.get_attr("path")?.coerce_to_source_path()?;
    let mut copy = TemplateCopy {
        accessor: &*accessor,
        notices: ctx.verbosity() >= LVL_NOTICE,
        changed: Vec::new(),
        conflicts: 0,
    };
    copy.copy_dir(&from, &template_path, &dest)?;
    let TemplateCopy {
        changed, conflicts, ..
    } = copy;
    if !changed.is_empty() && dest.join(".git").exists() {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(&dest)
            .args(["add", "--intent-to-add", "--force", "--"]);
        for f in &changed {
            cmd.arg(f);
        }
        let status = cmd
            .status()
            .map_err(|e| brix::Error::io("running git", e))?;
        if !status.success() {
            return Err(CliError::msg("'git add --intent-to-add' failed"));
        }
    }
    if let Some(welcome) = cursor.maybe_get_attr("welcomeText")? {
        eprintln!("\n");
        eprintln!("{}", welcome.get_string()?);
    }
    if conflicts > 0 {
        return Err(CliError::msg(format!(
            "encountered {conflicts} conflicts - see above"
        )));
    }
    Ok(())
}

/// `CmdFlakeInitCommon::run`'s copy loop: the template's files are copied
/// to the destination, which is created as needed, but never over a file
/// that differs (a conflict, reported and counted). As in Nix, files are
/// written with the default mode: the executable bit is not copied.
///
/// Names are bytes, as in Nix: a template's files are copied under the
/// names they have, whatever their bytes, in the template's directory
/// order (unsigned byte order), and every message shows them as they are.
struct TemplateCopy<'a> {
    accessor: &'a dyn brix_store::SourceAccessor,
    /// Whether `notice` messages are shown.
    notices: bool,
    changed: Vec<PathBuf>,
    conflicts: usize,
}

impl TemplateCopy<'_> {
    fn notice(&self, parts: &[&[u8]]) {
        if self.notices {
            brix::log::stderr_bytes(&parts.concat());
        }
    }

    fn conflict(&mut self, to: &Path, from_display: &[u8]) {
        // `printError`: an error-level message, without `error:`.
        brix::log::stderr_bytes(
            &[
                &b"refusing to overwrite existing file "[..],
                quote_path(to).as_slice(),
                b"\n please merge it manually with '",
                from_display,
                b"'",
            ]
            .concat(),
        );
        self.conflicts += 1;
    }

    fn copy_dir(
        &mut self,
        from: &brix_store::CanonPath,
        from_display: &[u8],
        to: &Path,
    ) -> Result<()> {
        use brix_store::FileType;
        let accessor_error = |e: brix_store::AccessorError| CliError::from(brix::Error::from(e));
        // `PathFmt(path)` after `what`, as the context of a file-system
        // error.
        let io_context =
            |what: &str, path: &Path| [what.as_bytes(), quote_path(path).as_slice()].concat();
        std::fs::create_dir_all(to)
            .map_err(|e| brix::Error::io(io_context("creating directory ", to), e))?;
        // In the order Nix's `readDirectory` gives them: unsigned bytes.
        let entries = self.accessor.read_dir(from).map_err(accessor_error)?;
        for name in entries.into_keys() {
            let from2 = from.join_component(&name).map_err(brix::Error::from)?;
            let from2_display = [from_display, b"/", name.as_slice()].concat();
            let to2 = to.join(OsStr::from_bytes(&name));
            let st = self.accessor.lstat(&from2).map_err(accessor_error)?;
            // `symlink_status`: whether anything is at `to2`, a link not
            // followed.
            let exists = match to2.symlink_metadata() {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    return Err(brix::Error::io(io_context("getting status of ", &to2), e).into())
                }
            };
            match st.file_type {
                // Then reported as written, as Nix reports a directory
                // (it does not skip the rest of the loop for one).
                FileType::Directory => self.copy_dir(&from2, &from2_display, &to2)?,
                FileType::Regular => {
                    let contents = self.accessor.read_file(&from2).map_err(accessor_error)?;
                    if exists {
                        let existing = std::fs::read(&to2)
                            .map_err(|e| brix::Error::io(io_context("opening file ", &to2), e))?;
                        if existing != contents {
                            self.conflict(&to2, &from2_display);
                        } else {
                            self.notice(&[b"skipping identical file: ", from2_display.as_slice()]);
                        }
                        continue;
                    }
                    std::fs::write(&to2, &contents)
                        .map_err(|e| brix::Error::io(io_context("opening file ", &to2), e))?;
                }
                FileType::Symlink => {
                    let target = self.accessor.read_link(&from2).map_err(accessor_error)?;
                    let target = Path::new(OsStr::from_bytes(&target));
                    if exists {
                        let existing = std::fs::read_link(&to2).map_err(|e| {
                            brix::Error::io(io_context("reading symbolic link ", &to2), e)
                        })?;
                        if existing != target {
                            self.conflict(&to2, &from2_display);
                        } else {
                            self.notice(&[b"skipping identical file: ", from2_display.as_slice()]);
                        }
                        continue;
                    }
                    std::os::unix::fs::symlink(target, &to2).map_err(|e| {
                        let context =
                            [io_context("creating symlink ", &to2), io_context(" -> ", target)]
                                .concat();
                        brix::Error::io(context, e)
                    })?;
                }
                other => {
                    return Err(CliError::msg(
                        [
                            &b"path '"[..],
                            from2_display.as_slice(),
                            format!(
                                "' needs to be a symlink, file, or directory but instead is a {other:?}"
                            )
                            .as_bytes(),
                        ]
                        .concat(),
                    ))
                }
            }
            self.notice(&[b"wrote: ", quote_path(&to2).as_slice()]);
            self.changed.push(to2);
        }
        Ok(())
    }
}

/// `brix flake compile`: a brix extension that compiles a flake and its
/// locked inputs into images (`brix_eval::image`), so later evaluations
/// link precompiled code instead of parsing. Images go to the cache, or
/// to `--out`.
pub fn compile(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let flake_ref = ctx.flake_ref(&session)?;
    let out_dir = ctx
        .inv
        .flag_value("out")
        .map(|p| fs_path(&ctx.abs_path(p)).to_path_buf());
    let only: Vec<String> = ctx
        .inv
        .flag_uses("input")
        .filter_map(|v| v.first().cloned())
        .collect();
    let with_tree = ctx.inv.has("with-tree");
    let flake = session.flake_with_flags(&flake_ref, &ctx.lock_flags(&session)?, true)?;
    let mut compiled = Vec::new();
    if !ctx.inv.has("no-self") && only.is_empty() {
        let image = session.compile_input(&flake_ref, out_dir.as_deref(), with_tree)?;
        compiled.push(("self".to_owned(), image));
    }
    let only = (!only.is_empty()).then_some(only.as_slice());
    compiled.extend(session.compile_flake_inputs(&flake, only, out_dir.as_deref(), with_tree)?);
    for (name, image) in &compiled {
        eprintln!(
            "compiled {name}: {} ({} modules, {} bytes, {:?})",
            image.path.display(),
            image.modules,
            image.bytes,
            image.duration
        );
    }
    if ctx.inv.has("json") {
        let json: Vec<Value> = compiled
            .iter()
            .map(|(name, image)| {
                json!({
                    "input": name,
                    "path": image.path,
                    "modules": image.modules,
                    "bytes": image.bytes,
                    "narHash": image.nar_hash.to_string(brix_store::HashFormat::Sri, true),
                })
            })
            .collect();
        ctx.print_json(&Value::Array(json))?;
    }
    Ok(())
}
