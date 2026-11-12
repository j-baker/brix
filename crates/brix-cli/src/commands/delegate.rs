//! The `nix` commands brix does not implement are run by Nix itself:
//! those that need a store of their own, profiles or the registry, and
//! the features brix leaves to Nix (`log`, `repl`, `why-depends`, …).
//! brix parses the command line first, so usage errors and `--help`
//! are its own, then replaces itself with the `nix` binary that
//! `BRIX_DELEGATE_NIX` names, passing the original arguments unchanged.
//! The brix package's wrapper sets the variable. Nix reads the same
//! `nix.conf` brix does for the store and daemon settings, so it works on
//! the store brix builds in.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use crate::args::Invocation;
use crate::settings::Settings;
use crate::tree::BRIX_ONLY_FLAGS;
use crate::CliError;

/// The environment variable naming the `nix` binary to delegate to.
pub const NIX_VAR: &str = "BRIX_DELEGATE_NIX";

/// Set to brix's process ID as it replaces itself with Nix. brix finding
/// its own ID there means [`NIX_VAR`] led back to brix (directly, through
/// a symlink or through brix's wrapper) in the same process, which would
/// otherwise repeat forever. The processes Nix starts have other IDs, so
/// brix run inside `nix develop` is unaffected.
const EXEC_MARKER: &str = "BRIX_DELEGATED_BY";

/// Replaces this process with Nix running `argv` (the command line after
/// the program name). Returns only if Nix could not be started.
pub fn exec(inv: &Invocation, settings: &Settings, argv: &[std::ffi::OsString]) -> CliError {
    let command = inv.path.join(" ");
    if let Some(flag) = BRIX_ONLY_FLAGS.iter().find(|f| inv.has(f)) {
        return CliError::usage(format!(
            "'--{flag}' is a brix option, but 'brix {command}' is run by Nix, which does not have it"
        ));
    }
    let nix = match std::env::var_os(NIX_VAR) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => {
            return config_error(format!(
                "'brix {command}' is run by Nix, but no Nix is configured: set {NIX_VAR} to the absolute path of a 'nix' binary"
            ))
        }
    };
    if !nix.is_absolute() {
        return config_error(format!(
            "{NIX_VAR} must be an absolute path, not '{}'",
            nix.display()
        ));
    }
    let pid = std::process::id().to_string();
    if std::env::var(EXEC_MARKER).is_ok_and(|marker| marker == pid) {
        return config_error(format!(
            "{NIX_VAR} ('{}') runs brix again, not Nix",
            nix.display()
        ));
    }
    if settings.verbosity > 0 {
        brix::log::stderr(&format!(
            "running 'brix {command}' with '{}'",
            nix.display()
        ));
    }
    let err = std::process::Command::new(&nix)
        .args(argv)
        .env(EXEC_MARKER, &pid)
        .exec();
    CliError::Brix(brix::Error::io(
        format!("unable to execute '{}'", nix.display()),
        err,
    ))
}

fn config_error(message: String) -> CliError {
    CliError::Brix(brix::Error::Config(message))
}
