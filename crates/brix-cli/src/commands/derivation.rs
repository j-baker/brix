//! `brix derivation show` and `brix derivation add`.

use crate::output::derivation_show_json;
use brix::StorePath;

use crate::commands::{print_stdout, Ctx};
use crate::{CliError, Result};

/// `nix derivation show`: the derivations behind the installables, with
/// `--recursive` their whole input closure.
pub fn show(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-build"])?;
    let installables = ctx.installables(&resolver)?;
    let roots: Vec<StorePath> = resolver.derivations(&installables)?.into_iter().collect();
    let drvs = if ctx.inv.has("recursive") {
        session.derivation_closure(&roots)?
    } else {
        // A set: sorted, each derivation once. A path that is not a
        // derivation (`out^out`) is left out, as Nix leaves it out.
        roots
            .into_iter()
            .filter(StorePath::is_derivation)
            .map(|p| Ok((p.clone(), session.read_derivation(&p)?)))
            .collect::<Result<Vec<_>>>()?
    };
    let json = derivation_show_json(&drvs)?;
    ctx.print_json(&json)
}

/// `nix derivation add`: a derivation in JSON on stdin, written to the
/// store (or only hashed with `--dry-run`).
pub fn add(ctx: &Ctx<'_>) -> Result<()> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        CliError::Brix(brix::Error::Usage(
            format!("invalid JSON on stdin: {e}").into(),
        ))
    })?;
    let session = ctx.session()?;
    let store_dir = session.config().store_dir().clone();
    let drv = session.derivation_from_json(&json)?;
    let path = if ctx.inv.has("dry-run") {
        drv.store_path(&store_dir).map_err(brix::Error::from)?
    } else {
        session.store_write_derivation(&drv)?
    };
    print_stdout(&format!("{}\n", store_dir.display(&path)))
}
