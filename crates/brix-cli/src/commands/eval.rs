//! `brix eval` (`src/nix/eval.cc`).

use std::path::Path;

use crate::installable::InstallableSource;
use brix::{Cursor, Format};

use crate::commands::realise::strerror;
use crate::commands::{print_stdout_bytes, Ctx};
use crate::{CliError, Result};

pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let raw = ctx.inv.has("raw");
    let json = ctx.inv.has("json");
    if raw && json {
        return Err(CliError::usage("--raw and --json are mutually exclusive"));
    }
    let drv_link = ctx.inv.flag_value("drv-link").map(str::to_owned);
    if drv_link.is_some() && !raw && !json {
        return Err(CliError::usage("--drv-link requires --raw or --json"));
    }
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-build"])?;
    let installable = ctx.installable(&resolver)?;
    // `InstallableValueCommand`: a store path has no value to evaluate.
    if matches!(installable.source, InstallableSource::StorePath(_)) {
        return Err(CliError::usage(format!(
            "installable '{}' does not correspond to a Nix language value",
            installable.what(session.config().store_dir())
        )));
    }
    let mut value = resolver.cursor(&installable)?;
    // `--apply`'s expression is bytes, its relative paths against the
    // current directory (`parseExprFromString(*apply, rootPath("."))`).
    if let Some(apply) = ctx.inv.flag_value_bytes("apply") {
        let fun = session.eval_expr_cursor(apply, &ctx.cwd)?;
        value = value.apply(&fun)?;
    }
    if let Some(dest) = ctx.inv.flag_value("write-to") {
        // The path as given, relative to the current directory.
        let path = Path::new(dest);
        // `pathExists`: an `lstat` that only a missing path fails.
        match path.symlink_metadata() {
            Ok(_) => return Err(CliError::msg(format!("path '{dest}' already exists"))),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => {
                return Err(CliError::msg(format!(
                    "getting status of \"{dest}\": {}",
                    strerror(&e)
                )))
            }
        }
        write_to(&value, path)?;
    } else if raw {
        let (s, _) = value.coerce_to_bytes()?;
        print_stdout_bytes(&s)?;
    } else if json {
        let text = value.render(Format::Json)?;
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| CliError::msg(format!("the evaluator produced invalid JSON: {e}")))?;
        ctx.print_json(&parsed)?;
    } else {
        let mut text = value.render_bytes(Format::Nix)?;
        text.push(b'\n');
        print_stdout_bytes(&text)?;
    }
    if let Some(link) = drv_link {
        drv_links(ctx, &session, &value, &link)?;
    }
    Ok(())
}

/// `--write-to`: a string becomes a file, an attribute set a directory.
fn write_to(value: &Cursor<'_>, path: &Path) -> Result<()> {
    use brix::ValueType;
    match value.value_type()? {
        ValueType::String => {
            let (s, _) = value.get_bytes_with_context()?;
            // `writeFile`.
            std::fs::write(path, s).map_err(|e| {
                CliError::msg(format!(
                    "opening file \"{}\": {}",
                    path.display(),
                    strerror(&e)
                ))
            })?;
        }
        ValueType::Attrs => {
            std::fs::create_dir(path).map_err(|e| {
                brix::Error::io(format!("creating directory '{}'", path.display()), e)
            })?;
            // In `Bindings` order, as Nix walks the set.
            for name in value.get_attrs_in_set_order()? {
                if name == "." || name == ".." {
                    return Err(CliError::msg(format!("invalid file name '{name}'")));
                }
                let child = value.get_attr(&name)?;
                write_to(&child, &path.join(&name)).map_err(|e| match e {
                    CliError::Message(mut m) => {
                        m.extend_from_slice(
                            format!("\n       … while evaluating the attribute '{name}'")
                                .as_bytes(),
                        );
                        CliError::Message(m)
                    }
                    other => other,
                })?;
            }
        }
        _ => {
            return Err(CliError::msg(format!(
                "value at '{}' is not a string or an attribute set",
                value.attr_path_str()
            )))
        }
    }
    Ok(())
}

/// `--drv-link`: symlinks to the derivations in the output's string
/// context, registered as GC roots, in a store on this machine only
/// (Nix's `LocalFSStore`).
fn drv_links(ctx: &Ctx<'_>, session: &brix::Session, value: &Cursor<'_>, link: &str) -> Result<()> {
    if !session.store_is_local() {
        return Ok(());
    }
    let (_, context) = value.coerce_to_bytes()?;
    let mut roots: Vec<brix::StorePath> = Vec::new();
    for elem in context {
        let path = match elem {
            brix::ContextElem::Opaque(path) => path,
            brix::ContextElem::DrvDeep(drv) | brix::ContextElem::Built { drv, .. } => drv,
            brix::ContextElem::Path(_) => continue,
        };
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots.sort();
    for (i, root) in roots.iter().enumerate() {
        let mut symlink = ctx.abs_path(link);
        if i > 0 {
            symlink.extend_from_slice(format!("-{i}").as_bytes());
        }
        // Nix's evaluation wrote the path to the store already, so a link
        // it may not make fails without the store being asked anything;
        // brix writes the path once the link is known to be possible.
        let symlink = crate::commands::build::perm_root_link(session, &symlink)?;
        crate::commands::realise::write_to_store(session, root)?;
        crate::commands::build::make_perm_root(session, root, &symlink)?;
    }
    Ok(())
}
