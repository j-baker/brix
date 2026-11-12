//! `brix fmt`, `brix formatter run` and `brix formatter build`: build the
//! current flake's `formatter.<system>` output and run it, or link to it
//! (`src/nix/formatter.cc`).

use std::ffi::OsStr;

use crate::installable::{Installable, InstallableSource, Resolver};

use crate::commands::build::{create_out_links, out_link};
use crate::commands::realise::{exec_program, process_env, require_local_store, resolve_app};
use crate::commands::{print_stdout, Ctx};
use crate::{CliError, Result};

/// `MixFormatter::getRoles`: the flake schemas' `nix-fmt` role selects
/// `formatter.<system>`.
const ROLES: [&str; 1] = ["nix-fmt"];

/// `brix fmt` and `brix formatter run` (`CmdFormatterRun::run`): build the
/// formatter and execute it with the command's arguments, `PRJ_ROOT` set to
/// the flake's source directory so that it can format the whole project
/// from a subdirectory.
pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &ROLES)?;
    let installable = current_flake(&resolver)?;
    // `.cast<InstallableFlake>()`: with `--file` or `--expr`, `.` is an
    // attribute path instead, and Nix's cast fails with this message.
    let InstallableSource::Flake { flake_ref, .. } = &installable.source else {
        return Err(CliError::msg(
            "ref<nix::Installable> cannot be cast to ref<nix::InstallableFlake>",
        ));
    };
    let app = resolver.app(&installable)?;
    resolve_app(ctx, &session, &app)?;
    // Nix asserts that `.` has a source path; it always does (a `path:` or
    // `git+file:` reference), so its absence is a bug worth reporting.
    let flake_dir = brix_flake::native::source_path(&flake_ref.input)
        .map_err(brix::Error::Flake)?
        .ok_or_else(|| {
            CliError::msg(format!(
                "the flake reference '{flake_ref}' has no source directory to set PRJ_ROOT to"
            ))
        })?;
    // `getEnv()` with `PRJ_ROOT` added, a `StringMap`, so the formatter
    // gets the variables in name order; `fmt` has no environment options.
    let mut env = process_env();
    env.set("PRJ_ROOT", flake_dir.into_os_string());
    let env = env.sorted();
    // `execProgramInStore` without a lookup path: argv[0] is the program.
    require_local_store(&session)?;
    let program = OsStr::new(&app.program);
    Err(exec_program(program, program, ctx.inv.arg("args"), &env))
}

/// `brix formatter build` (`CmdFormatterBuild::run`): build the formatter,
/// link to it as `nix build` does (`result` by default) and print the path
/// of its program.
pub fn build(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &ROLES)?;
    let installable = current_flake(&resolver)?;
    let app = resolver.app(&installable)?;
    // Nix resolves the app (building its context) and then builds the
    // context again for the links; the second build finds everything valid
    // and reports nothing, so one build serves both.
    let built = resolve_app(ctx, &session, &app)?;
    if let Some(link) = out_link(ctx) {
        create_out_links(&session, ctx.abs_path(link), &built)?;
    }
    print_stdout(&format!("{}\n", app.program))
}

/// `parseInstallable(store, ".")`: the formatter always comes from the flake
/// in the current directory; the commands take no installable.
fn current_flake(resolver: &Resolver<'_>) -> Result<Installable> {
    Ok(resolver.parse(&[".".to_owned()])?.remove(0))
}
