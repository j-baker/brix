//! `brix bundle`: hand an installable's value to a bundler function from a
//! flake and build the derivation it returns (`src/nix/bundle.cc`).

use crate::installable::{parse_outputs_spec, DerivedPath, Installable, InstallableSource};
use brix::store::BuildMode;
use brix::{LockFlags, Session, StorePath};

use crate::commands::build::add_perm_root;
use crate::commands::Ctx;
use crate::{CliError, Result};

/// `CmdBundle::bundler`'s default.
const DEFAULT_BUNDLER: &str = "github:NixOS/bundlers";

/// `brix bundle` (`CmdBundle::run`).
pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-run"])?;
    let installable = ctx.installable(&resolver)?;
    // `InstallableValueCommand`: a store path has no value to bundle.
    if matches!(installable.source, InstallableSource::StorePath(_)) {
        return Err(CliError::usage(format!(
            "installable '{}' does not correspond to a Nix language value",
            installable.what(session.config().store_dir())
        )));
    }
    // `installable->toValue(*evalState)`: not forced; the bundler decides.
    let value = resolver.cursor(&installable)?;

    // `parseFlakeRefWithFragmentAndExtendedOutputsSpec`: the outputs
    // specifier is parsed and then ignored.
    let bundler_text = ctx.inv.flag_value("bundler").unwrap_or(DEFAULT_BUNDLER);
    let (prefix, outputs) = parse_outputs_spec(bundler_text)?;
    let (flake_ref, fragment) = session.parse_flake_ref_with_fragment(prefix, ctx.cwd_text()?)?;
    // `InstallableFlake::what()`.
    let what = format!("{flake_ref}#{fragment}");
    let mut bundler_resolver =
        ctx.resolver_with(&session, bundler_lock_flags(ctx)?, &["nix-bundler"])?;
    // `lockFlake` with `writeLockFile = false`: a lock file that needs
    // changes is changed in memory only, and Nix says so (brix-flake warns).
    bundler_resolver.flake(&flake_ref)?;
    let bundler = Installable {
        source: InstallableSource::Flake {
            flake_ref,
            fragment,
        },
        outputs,
        text: bundler_text.into(),
    };
    let function = bundler_resolver.cursor(&bundler)?;

    // `callFunction`, then the result must be a derivation.
    let result = value.apply(&function)?;
    let not_a_derivation = || {
        CliError::msg(format!(
            "the bundler '{what}' does not produce a derivation"
        ))
    };
    if !result.is_derivation()? {
        return Err(not_a_derivation());
    }
    let drv_path = result
        .coerce_attr_to_store_path("drvPath", "")?
        .ok_or_else(not_a_derivation)?;
    // `StorePath::requireDerivation`.
    if !drv_path.is_derivation() {
        return Err(CliError::msg(format!(
            "store path '{}' is not a valid derivation path",
            drv_path.as_str()
        )));
    }
    let out_path = result
        .coerce_attr_to_store_path("outPath", "")?
        .ok_or_else(not_a_derivation)?;

    build_all_outputs(&session, &drv_path)?;

    let out_link = match ctx.inv.flag_value("out-link") {
        Some(link) => link.to_owned(),
        None => result
            .force_attr_string_no_ctx("name", "")?
            .ok_or_else(|| CliError::msg("attribute 'name' missing"))?,
    };
    add_perm_root(&session, &out_path, ctx.abs_path(&out_link))
}

/// The lock flags of the bundler's flake: Nix's defaults with
/// `writeLockFile = false`. The command's flake options (`--override-input`,
/// `--no-update-lock-file`, …) apply to the installable only, and
/// `useRegistries` is left to the `use-registries` setting.
fn bundler_lock_flags(ctx: &Ctx<'_>) -> Result<LockFlags> {
    Ok(LockFlags {
        write_lock_file: false,
        use_registries: ctx.settings.get_bool("use-registries", true)?,
        ..LockFlags::default()
    })
}

/// `Store::buildPaths` of every output of `drv_path`. Unlike `nix build`,
/// `nix bundle` prints neither what will be built nor a ✅/❌ report: a
/// failure is the daemon's error for the derivation, reported like any
/// other.
fn build_all_outputs(session: &Session, drv_path: &StorePath) -> Result<()> {
    let request = DerivedPath::all_outputs(drv_path.clone());
    let results = session.store_realise(std::slice::from_ref(&request), BuildMode::Normal)?;
    let store_dir = session.config().store_dir();
    match results.iter().find(|r| r.path == request) {
        Some(r) if r.result.is_success() => Ok(()),
        Some(r) => Err(CliError::msg(r.result.error_msg.clone())),
        None => Err(CliError::msg(format!(
            "the daemon reported no result for building '{}^*'",
            store_dir.display(drv_path)
        ))),
    }
}
