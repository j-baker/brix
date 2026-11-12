//! What the commands that build share: turning installables into requests,
//! building them with Nix's report of what failed (`Installable::build`,
//! `throwBuildErrors`), the paths that results in, resolving an app
//! (`UnresolvedApp::resolve`), and running a program from the store with
//! the environment the options ask for (`MixEnvironment`,
//! `execProgramInStore`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};

use bstr::BString;

use crate::installable::{DerivedPath, Installable, OutputsSpec, Resolver, UnresolvedApp};
use brix::store::{self as store, BuildMode, BuildStatus, KeyedBuildResult};
use brix::{Session, StoreDir, StorePath};

use crate::args::Invocation;
use crate::commands::path_info::render_size;
use crate::commands::Ctx;
use crate::{CliError, Result};

const ANSI_NORMAL: &str = "\u{1b}[0m";
const ANSI_BOLD: &str = "\u{1b}[1m";
const ANSI_FAINT: &str = "\u{1b}[2m";
const ANSI_RED: &str = "\u{1b}[31;1m";

/// What to build, each with the text that names it in the report (Nix's
/// `Installable::what()`): bytes, as a derived path's output names are.
pub type Requests = Vec<(BString, DerivedPath)>;

/// The derived paths of `installables`, each named by its installable.
pub fn requests_for(resolver: &mut Resolver<'_>, installables: &[Installable]) -> Result<Requests> {
    let store_dir = resolver.session().config().store_dir().clone();
    let mut requests = Vec::new();
    for i in installables {
        let what = BString::from(i.what(&store_dir));
        for d in resolver.derived_paths(i)? {
            requests.push((what.clone(), d));
        }
    }
    Ok(requests)
}

/// A request for `path` named `what`, from no flake output.
pub fn request(what: impl Into<BString>, path: DerivedPath) -> (BString, DerivedPath) {
    (what.into(), path)
}

/// One `InstallableDerivedPath` per path, named by the path itself
/// (`DerivedPath::to_string`, bytes).
pub fn requests_for_paths(store_dir: &StoreDir, paths: &[DerivedPath]) -> Requests {
    paths
        .iter()
        .map(|path| request(path.display(store_dir), path.clone()))
        .collect()
}

/// The output paths of a derivation (`queryPartialDerivationOutputMap`
/// restricted to `outputs`), by output name (bytes): from the evaluation
/// when brix produced the derivation, otherwise from the daemon's copy.
pub fn output_paths_of(
    session: &Session,
    drv_path: &StorePath,
    outputs: &OutputsSpec,
) -> Result<Vec<(BString, StorePath)>> {
    // `resolveDerivedPath` reads it with `readInvalidDerivation`.
    let drv = session.read_invalid_derivation(drv_path)?;
    let mut out = Vec::new();
    for (name, output) in &drv.outputs {
        if !outputs.contains(name) {
            continue;
        }
        match output {
            brix::DerivationOutput::InputAddressed(p) => out.push((name.clone(), p.clone())),
            _ => {
                // brix's own refusal (`KNOWN_DEVIATIONS.md`), naming the
                // output for a reader.
                return Err(CliError::msg(format!(
                    "output '{}' of '{}' has no output path known in advance (content-addressed outputs are not supported by brix)",
                    bstr::BStr::new(name),
                    session.config().store_dir().display(drv_path)
                )));
            }
        }
    }
    Ok(out)
}

/// Writes `path` to the store, with its closure, if evaluation produced it
/// (a derivation, a `toFile` text, a source): Nix's evaluation writes
/// every one as it makes it, brix's only when the store needs it.
pub fn write_to_store(session: &Session, path: &StorePath) -> Result<()> {
    Ok(session.store_upload(std::slice::from_ref(path))?)
}

/// `Installable::toStorePaths` with `Realise::Nothing`: what building
/// would do (`printMissing`), then the paths `derived` denotes, outputs as
/// they would be built, in order.
pub fn to_store_paths(session: &Session, derived: &[DerivedPath]) -> Result<Vec<StorePath>> {
    print_missing(session, derived)?;
    let mut paths = Vec::new();
    for d in derived {
        match d {
            DerivedPath::Opaque(p) => paths.push(p.clone()),
            DerivedPath::Built { drv_path, outputs } => paths.extend(
                output_paths_of(session, drv_path, outputs)?
                    .into_iter()
                    .map(|(_, p)| p),
            ),
        }
    }
    Ok(paths)
}

/// `printMissing`: what a build would do, on stderr.
pub fn print_missing(session: &Session, paths: &[DerivedPath]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let missing = session.store_query_missing(paths)?;
    let store_dir = session.config().store_dir().clone();
    if !missing.will_build.is_empty() {
        if missing.will_build.len() == 1 {
            eprintln!("this derivation will be built:");
        } else {
            eprintln!(
                "these {} derivations will be built:",
                missing.will_build.len()
            );
        }
        let sorted = session.sort_derivations(&missing.will_build);
        // `topoSortPaths` yields dependents first; Nix prints it reversed,
        // so dependencies come first.
        let mut listed: BTreeSet<StorePath> = BTreeSet::new();
        for p in &sorted {
            eprintln!("  {}", store_dir.display(p));
            listed.insert(p.clone());
        }
        for p in &missing.will_build {
            if !listed.contains(p) {
                eprintln!("  {}", store_dir.display(p));
            }
        }
    }
    if !missing.will_substitute.is_empty() {
        if missing.will_substitute.len() == 1 {
            eprintln!(
                "this path will be fetched ({} download, {} unpacked):",
                render_size(missing.download_size, false),
                render_size(missing.nar_size, false)
            );
        } else {
            eprintln!(
                "these {} paths will be fetched ({} download, {} unpacked):",
                missing.will_substitute.len(),
                render_size(missing.download_size, false),
                render_size(missing.nar_size, false)
            );
        }
        let mut sorted: Vec<&StorePath> = missing.will_substitute.iter().collect();
        sorted.sort_by(|a, b| {
            a.name()
                .cmp(b.name())
                .then_with(|| a.as_str().cmp(b.as_str()))
        });
        for p in sorted {
            eprintln!("  {}", store_dir.display(p));
        }
    }
    if !missing.unknown.is_empty() {
        eprintln!("don't know how to build these paths:");
        for p in &missing.unknown {
            eprintln!("  {}", store_dir.display(p));
        }
    }
    Ok(())
}

/// What `Installable::build` returns for one request.
pub struct Built {
    /// `what()`: bytes, as a derived path's output names are.
    pub installable_text: BString,
    pub request: DerivedPath,
    /// Output name (bytes) → path (`out` → path for opaque paths is keyed
    /// `""`).
    pub outputs: Vec<(BString, StorePath)>,
    pub result: Option<store::BuildResult>,
}

/// `Installable::build` with `Realise::Outputs`: what will be built
/// (`printMissing`, unless the `print-missing` setting is off), then
/// [`realise`].
pub fn build_requests(
    ctx: &Ctx<'_>,
    session: &Session,
    requests: &Requests,
    mode: BuildMode,
) -> Result<Vec<Built>> {
    if ctx.settings.get_bool("print-missing", true)? {
        let paths: Vec<DerivedPath> = requests.iter().map(|(_, r)| r.clone()).collect();
        print_missing(session, &paths)?;
    }
    realise(session, requests, mode)
}

/// `BuildResult::isCancelled`: a goal that never ran because another
/// failed. Daemons omit the results of such goals, or report them as
/// cancelled (`MiscFailure` being the older word for it).
fn is_cancelled(result: Option<&KeyedBuildResult>) -> bool {
    result.is_none_or(|r| {
        matches!(
            r.result.status,
            BuildStatus::Cancelled | BuildStatus::MiscFailure
        )
    })
}

/// Builds every request and reports failures the way Nix's
/// `throwBuildErrors` does: the successes (✅), the cancelled builds (❓)
/// and each failure (❌, then its error), exiting 1 on any failure.
pub fn realise(session: &Session, requests: &Requests, mode: BuildMode) -> Result<Vec<Built>> {
    let mut distinct: Vec<DerivedPath> = Vec::new();
    for (_, r) in requests {
        if !distinct.contains(r) {
            distinct.push(r.clone());
        }
    }
    let results: Vec<KeyedBuildResult> = session.store_realise(&distinct, mode)?;
    let mut out = Vec::new();
    let mut failures: Vec<(&[u8], Option<&KeyedBuildResult>)> = Vec::new();
    for (text, req) in requests {
        let Some(r) = results.iter().find(|r| r.path == *req) else {
            failures.push((text.as_slice(), None));
            continue;
        };
        if !r.result.is_success() {
            failures.push((text.as_slice(), Some(r)));
            continue;
        }
        let outputs = match req {
            DerivedPath::Opaque(p) => vec![(BString::default(), p.clone())],
            DerivedPath::Built { drv_path, outputs } => {
                if r.result.built_outputs.is_empty() {
                    output_paths_of(session, drv_path, outputs)?
                } else {
                    r.result
                        .built_outputs
                        .iter()
                        .filter(|(n, _)| outputs.contains(n))
                        .map(|(n, p)| (n.clone(), p.clone()))
                        .collect()
                }
            }
        };
        out.push(Built {
            installable_text: text.clone(),
            request: req.clone(),
            outputs,
            result: Some(r.result.clone()),
        });
    }
    if failures.is_empty() {
        return Ok(out);
    }
    // The texts are bytes (a derived path's output names), written as
    // they are.
    let line = |parts: &[&[u8]]| brix::log::stderr_bytes(&parts.concat());
    for b in &out {
        line(&[
            "✅ ".as_bytes(),
            ANSI_BOLD.as_bytes(),
            b.installable_text.as_slice(),
            ANSI_NORMAL.as_bytes(),
        ]);
    }
    for &(text, r) in &failures {
        if is_cancelled(r) {
            line(&[
                "❓ ".as_bytes(),
                ANSI_BOLD.as_bytes(),
                text,
                ANSI_NORMAL.as_bytes(),
                ANSI_FAINT.as_bytes(),
                " (cancelled)".as_bytes(),
            ]);
        }
    }
    for &(text, r) in &failures {
        match r {
            Some(r) if !is_cancelled(Some(r)) => {
                line(&[
                    "❌ ".as_bytes(),
                    ANSI_RED.as_bytes(),
                    text,
                    ANSI_NORMAL.as_bytes(),
                ]);
                crate::log_error(&r.result.error_msg);
            }
            _ => {}
        }
    }
    Err(CliError::Exit(1))
}

/// `UnresolvedApp::resolve`: build what the app's program needs
/// (`UnresolvedApp::build`: `Installable::build` of one
/// `InstallableDerivedPath` per context element) and check that the
/// program is in the store.
pub fn resolve_app(ctx: &Ctx<'_>, session: &Session, app: &UnresolvedApp) -> Result<Vec<Built>> {
    let store_dir = session.config().store_dir();
    let requests = requests_for_paths(store_dir, &app.context);
    let built = build_requests(ctx, session, &requests, BuildMode::Normal)?;
    if !is_in_store(store_dir, &app.program) {
        return Err(CliError::msg(format!(
            "app program '{}' is not in the Nix store",
            app.program
        )));
    }
    Ok(built)
}

/// `StoreDirConfig::isInStore`: whether `path` (bytes, as Nix's paths
/// are) is below the store directory.
pub fn is_in_store(store_dir: &StoreDir, path: impl AsRef<[u8]>) -> bool {
    store_dir.is_in_store(path)
}

/// A path the command line works with (bytes, as Nix's paths are: the
/// current directory, a path argument made absolute) as the operating
/// system's. brix's command line runs on Unix, where any bytes name a
/// file.
pub fn fs_path(path: &(impl AsRef<[u8]> + ?Sized)) -> &std::path::Path {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(OsStr::from_bytes(path.as_ref()))
}

/// A process environment in `environ` order: like Nix, brix passes on
/// variables that are not UTF-8, and the order is what the program sees
/// (`env` prints it). Setting a variable keeps its place, and a new one
/// goes at the end, as `setenv` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment(Vec<(OsString, OsString)>);

impl Environment {
    /// The environment brix was started with, in its order. Of a name
    /// that appears twice, the first value counts, as for `getenv`.
    pub fn current() -> Environment {
        let mut env = Environment::default();
        for (name, value) in std::env::vars_os() {
            if env.get(&name).is_none() {
                env.0.push((name, value));
            }
        }
        env
    }

    pub fn get(&self, name: &OsStr) -> Option<&OsStr> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_os_str())
    }

    /// `setenv`: the variable's value replaced where it is, or the
    /// variable added at the end.
    pub fn set(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        let (name, value) = (name.into(), value.into());
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => *v = value,
            None => self.0.push((name, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.0.iter().map(|(n, v)| (n.as_os_str(), v.as_os_str()))
    }

    /// The variables in name order, as a `StringMap` (`std::map`) of
    /// the environment hands them to `execve`.
    pub fn sorted(mut self) -> Environment {
        self.0.sort_by(|a, b| a.0.cmp(&b.0));
        self
    }
}

/// `getEnv()`: the environment brix was started with.
pub fn process_env() -> Environment {
    Environment::current()
}

/// `MixEnvironment`: `--ignore-env`, `--keep-env-var`, `--unset-env-var`
/// and `--set-env-var`.
#[derive(Debug, Default)]
pub struct MixEnvironment {
    ignore: bool,
    keep: BTreeSet<OsString>,
    unset: BTreeSet<OsString>,
    set: BTreeMap<OsString, OsString>,
}

impl MixEnvironment {
    /// The flags, checked in command-line order as their handlers check
    /// them while Nix parses the command line: before anything else
    /// runs.
    pub fn from_flags(inv: &Invocation) -> Result<MixEnvironment> {
        let mut env = MixEnvironment::default();
        for flag in &inv.flags {
            match flag.name.as_str() {
                "ignore-env" => env.ignore = true,
                "keep-env-var" => {
                    env.keep.insert(flag.values[0].clone().into());
                }
                "unset-env-var" => {
                    let name = &flag.values[0];
                    if env.set.contains_key(OsStr::new(name)) {
                        return Err(CliError::usage(format!(
                            "Cannot unset environment variable '{name}' that is set with '--set-env-var'"
                        )));
                    }
                    env.unset.insert(name.into());
                }
                "set-env-var" => {
                    let (name, value) = (&flag.values[0], &flag.values[1]);
                    if env.unset.contains(OsStr::new(name)) {
                        return Err(CliError::usage(format!(
                            "Cannot set environment variable '{name}' that is unset with '--unset-env-var'"
                        )));
                    }
                    if env.set.contains_key(OsStr::new(name)) {
                        return Err(CliError::usage(format!(
                            "Duplicate definition of environment variable '{name}' with '--set-env-var' is ambiguous"
                        )));
                    }
                    env.set.insert(name.into(), value.into());
                }
                _ => {}
            }
        }
        Ok(env)
    }

    /// `setEnviron`: the process environment as the flags change it,
    /// which the program then runs with. Nix replaces its own with the
    /// variables of a sorted map (`replaceEnv`), so they come in name
    /// order.
    pub fn environ(&self) -> Result<Environment> {
        if self.ignore && !self.unset.is_empty() {
            return Err(CliError::usage(
                "--unset-env-var does not make sense with --ignore-env",
            ));
        }
        if !self.ignore && !self.keep.is_empty() {
            return Err(CliError::usage(
                "--keep-env-var does not make sense without --ignore-env",
            ));
        }
        let mut env: BTreeMap<OsString, OsString> = Environment::current().0.into_iter().collect();
        if self.ignore {
            env.retain(|name, _| self.keep.contains(name));
        }
        for (name, value) in &self.set {
            env.insert(name.clone(), value.clone());
        }
        env.retain(|name, _| !self.unset.contains(name));
        Ok(Environment(env.into_iter().collect()))
    }
}

/// `execProgramInStore`'s refusal of stores that are not on this machine.
pub fn require_local_store(session: &Session) -> Result<()> {
    if !session.store_is_local() {
        return Err(CliError::msg(format!(
            "store '{}' is not a local store so it does not support command execution",
            session
                .config()
                .daemon
                .clone()
                .unwrap_or_else(|| "dummy://".into())
        )));
    }
    Ok(())
}

/// The end of `execProgramInStore`: replaces this process with
/// `program`, run as `arg0` with `args` and exactly the environment `env`,
/// in its order (`std::process::Command` would sort it). A `program`
/// without a `/` is looked up in `env`'s `PATH`, as `execvp` looks it up
/// in the environment Nix has just set. Returns only if the program could
/// not be started.
pub fn exec_program(
    program: &OsStr,
    arg0: &OsStr,
    args: &[impl AsRef<OsStr>],
    env: &Environment,
) -> CliError {
    use std::os::unix::ffi::OsStrExt;
    let err = execvpe(program, arg0, args, env);
    // The program's name as it is (`shell` runs one from `PATH` or from a
    // store path's `bin`, whose name is bytes).
    CliError::msg(
        [
            &b"unable to execute '"[..],
            program.as_bytes(),
            format!("': {}", strerror(&err)).as_bytes(),
        ]
        .concat(),
    )
}

/// `execvp` with the environment `env`: `execve` of `program`, or of each
/// `PATH` candidate in turn (glibc's default path without a `PATH`, `.`
/// for an empty entry) until one starts. The error is the last one that
/// says something, or "permission denied" if any candidate said that.
fn execvpe(
    program: &OsStr,
    arg0: &OsStr,
    args: &[impl AsRef<OsStr>],
    env: &Environment,
) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |s: &OsStr| CString::new(s.as_bytes());
    let argv: std::result::Result<Vec<CString>, _> = std::iter::once(arg0)
        .chain(args.iter().map(AsRef::as_ref))
        .map(c)
        .collect();
    let envp: std::result::Result<Vec<CString>, _> = env
        .iter()
        .map(|(name, value)| {
            let mut entry = name.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry)
        })
        .collect();
    let (Ok(argv), Ok(envp)) = (argv, envp) else {
        return std::io::Error::from_raw_os_error(libc::EINVAL);
    };
    let execve = |path: &[u8]| -> std::io::Error {
        let Ok(path) = CString::new(path) else {
            return std::io::Error::from_raw_os_error(libc::ENOENT);
        };
        match nix::unistd::execve(&path, &argv, &envp) {
            Ok(never) => match never {},
            Err(errno) => errno.into(),
        }
    };
    if program.as_bytes().contains(&b'/') {
        return execve(program.as_bytes());
    }
    let search = env
        .get(OsStr::new("PATH"))
        .map_or(&b"/bin:/usr/bin"[..], |p| p.as_bytes());
    let mut denied = false;
    let mut last = std::io::Error::from_raw_os_error(libc::ENOENT);
    for dir in search.split(|&b| b == b':') {
        let dir = if dir.is_empty() { &b"."[..] } else { dir };
        let mut candidate = dir.to_vec();
        candidate.push(b'/');
        candidate.extend_from_slice(program.as_bytes());
        let err = execve(&candidate);
        match err.raw_os_error() {
            Some(libc::EACCES) => denied = true,
            Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ENODEV | libc::ETIMEDOUT) => {}
            _ => last = err,
        }
    }
    if denied {
        std::io::Error::from_raw_os_error(libc::EACCES)
    } else {
        last
    }
}

/// The text `SysError` adds for an OS error (`strerror`), without the
/// `(os error N)` Rust appends.
pub fn strerror(e: &std::io::Error) -> String {
    brix::error::sys_error_text(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_in_the_store() {
        let store_dir = StoreDir::new("/nix/store");
        assert!(is_in_store(&store_dir, "/nix/store/abc-x"));
        assert!(is_in_store(&store_dir, "/nix/store/abc-x/bin/x"));
        assert!(!is_in_store(&store_dir, "/nix/store"));
        assert!(!is_in_store(&store_dir, "/nix/store/"));
        assert!(!is_in_store(&store_dir, "/nix/storex/abc-x"));
        assert!(!is_in_store(&store_dir, "abc-x"));
    }

    /// `setenv`: a variable set again keeps its place, a new one goes at
    /// the end.
    #[test]
    fn environments_keep_setenv_order() {
        let mut env = Environment::default();
        env.set("B", "1");
        env.set("A", "2");
        env.set("B", "3");
        env.set("C", "4");
        let order: Vec<(&OsStr, &OsStr)> = env.iter().collect();
        assert_eq!(
            order,
            [
                (OsStr::new("B"), OsStr::new("3")),
                (OsStr::new("A"), OsStr::new("2")),
                (OsStr::new("C"), OsStr::new("4")),
            ]
        );
        assert_eq!(env.get(OsStr::new("A")), Some(OsStr::new("2")));
        assert_eq!(env.get(OsStr::new("D")), None);
    }

    /// Exercise exec in a subprocess: PATH comes from the replacement
    /// environment, and execve must pass that environment in its order.
    #[test]
    fn exec_searches_the_new_path_and_preserves_environment_order() {
        const CHILD_DIR: &str = "BRIX_TEST_EXEC_DIR";
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let mut env = Environment::default();
            env.set("ZB", "1");
            env.set("AB", "2");
            env.set("PATH", dir);
            let error = execvpe(
                OsStr::new("brix-test-env"),
                OsStr::new("env"),
                &[] as &[&OsStr],
                &env,
            );
            panic!("exec failed: {error}");
        }

        let env_program = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("env"))
            .find(|path| path.is_file())
            .expect("env is on PATH")
            .canonicalize()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(env_program, dir.path().join("brix-test-env")).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::realise::tests::exec_searches_the_new_path_and_preserves_environment_order",
                "--nocapture",
            ])
            .env(CHILD_DIR, dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        // The test harness prints a prelude before the process is replaced.
        let expected = format!("ZB=1\nAB=2\nPATH={}\n", dir.path().display());
        assert!(output.stdout.ends_with(expected.as_bytes()), "{output:?}");
    }

    #[test]
    fn os_errors_read_like_strerror() {
        let e = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(strerror(&e), "No such file or directory");
        let e = std::io::Error::other("custom (os error 1) text");
        assert_eq!(strerror(&e), "custom (os error 1) text");
    }
}
