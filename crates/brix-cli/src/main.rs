//! `brix`: the `nix` command line, evaluated by brix and built through a
//! Nix daemon. The command tree, flags and output formats are `nix`'s; see
//! `tree.rs` for where the structure comes from.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(not(brix_parallel))]
use std::process::ExitCode;

use bstr::BString;

mod args;
mod commands;
mod descriptions;
mod installable;
mod internet;
mod output;
mod progress;
mod schemas;
mod settings;
#[cfg(not(brix_parallel))]
mod signals;
mod tree;

use args::UsageError;
use settings::Settings;

// mimalloc for the many small temporaries outside the evaluator's arena,
// and the arena's chunks pre-faulted off the critical path (see
// `brix_alloc`). `BRIX_PREFAULT=off|sync|background` selects the policy.
#[cfg(not(brix_parallel))]
brix_alloc::install!();

/// What a command can fail with.
#[derive(Debug)]
pub enum CliError {
    /// Nix's `UsageError`: `error: <message>`, then the "Try '<program>
    /// --help'" hint (`handleExceptions` in `shared.cc`). Bytes: it can
    /// quote a value (`brix::Error::Usage`).
    UsageWithHint(BString),
    /// Nix's plain `Error` with a message of the command's own:
    /// `error: <message>`.
    Message(BString),
    /// An error from the library, printed as it renders itself.
    Brix(brix::Error),
    /// Exit with this status without printing anything more.
    Exit(i32),
}

impl CliError {
    /// A `UsageError`, reported with the "Try --help" hint.
    pub fn usage(message: impl Into<BString>) -> CliError {
        CliError::UsageWithHint(message.into())
    }

    /// A plain `Error`, reported as `error: <message>`.
    pub fn msg(message: impl Into<BString>) -> CliError {
        CliError::Message(message.into())
    }

    /// What brix cannot do, reported as `error: <message>`. Bytes: it can
    /// quote an argument, which need not be UTF-8.
    pub fn unsupported(message: impl Into<BString>) -> CliError {
        CliError::Brix(brix::Error::Usage(message.into()))
    }
}

impl From<brix::Error> for CliError {
    fn from(e: brix::Error) -> Self {
        CliError::Brix(e)
    }
}

impl From<UsageError> for CliError {
    fn from(e: UsageError) -> Self {
        CliError::UsageWithHint(e.0)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Brix(brix::Error::Io {
            context: "I/O error".into(),
            source: e,
        })
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

/// Nix evaluates deep expressions; give the evaluator a generous stack
/// (the evaluator also grows it on demand, this is the first segment).
#[cfg(not(brix_parallel))]
const STACK_SIZE: usize = 256 * 1024 * 1024;

#[cfg(not(brix_parallel))]
fn main() -> ExitCode {
    if let Err(e) = signals::init() {
        eprintln!("error: installing interrupt handler: {e}");
        return ExitCode::from(1);
    }
    // Arguments are bytes, as for Nix: none may make brix panic.
    let mut args = std::env::args_os();
    let program: Vec<u8> = args
        .next()
        .map_or_else(|| b"brix".to_vec(), OsString::into_vec);
    let argv: Vec<OsString> = args.collect();
    if let Err(e) = brix_alloc::init() {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    let handle = std::thread::Builder::new()
        .name("brix-main".into())
        .stack_size(STACK_SIZE)
        .spawn(move || run(&program, &argv))
        .expect("spawn the main thread");
    match handle.join() {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => ExitCode::from(e as u8),
        Err(_) => ExitCode::from(1),
    }
}

/// The threads `flake check` evaluates on ([`commands::eval_workers`]).
/// `None` if the configuration cannot be read: the command reports that
/// itself.
#[cfg(not(brix_parallel))]
fn flake_check_workers(settings: &Settings) -> Option<u32> {
    let mut conf = brix::nixconf::NixConf::load().ok()?;
    settings.apply_to(&mut conf);
    commands::eval_workers(&conf).ok()
}

/// Parses the command line, runs the command, and reports errors the way
/// `nix` does. Returns the exit status on failure.
///
/// The arguments are bytes, as Nix's are. A command Nix runs gets them as
/// they were. brix reads the expressions, files and installables Nix
/// reads as bytes as bytes too ([`commands::BYTE_FLAGS`],
/// [`commands::BYTE_ARGS`]), and the rest as text: one of those that is
/// not UTF-8 is refused, by name, rather than acted on altered.
#[doc(hidden)]
pub fn run(program: &[u8], raw_argv: &[OsString]) -> std::result::Result<(), i32> {
    let argv: Vec<&[u8]> = raw_argv.iter().map(|a| a.as_bytes()).collect();
    let root = tree::build();
    let aliases = tree::aliases();
    // `nix __dump-cli` and `nix --version` are answered before any other
    // parsing, as in `main.cc`.
    if argv.len() == 1 && argv[0] == b"__dump-cli" {
        println!("{}", commands::help::dump_cli(&root));
        return Ok(());
    }
    let (inv, parse_error) = match args::parse(&root, &aliases, &argv) {
        Ok(inv) => (inv, None),
        Err(failure) => (failure.parsed, Some(failure.error)),
    };
    // The settings of the flags read, in order, as Nix's flag handlers
    // apply them while it parses: a setting flag's bad value is reported
    // before an error further along the command line.
    let mut settings = Settings::from_invocation(&root, &inv);
    #[cfg(not(brix_parallel))]
    if parse_error.is_none()
        && settings.error.is_none()
        && !settings.help
        && !settings.version
        && inv.path == ["flake", "check"]
        && flake_check_workers(&settings).is_some_and(|workers| workers != 1)
    {
        return brix_cli_parallel::run(program, raw_argv);
    }
    // A command brix does not implement is Nix's: Nix gets the original
    // command line and prints its own warnings about it.
    let delegated = parse_error.is_none() && commands::delegated(&root, &inv.path);
    if !delegated {
        // What brix reads as text must be UTF-8: refused before anything
        // (a setting's warning or error) shows it altered. A usage error
        // is Nix's to report first, and help or the version reads no
        // value.
        if parse_error.is_none() && !settings.help && !settings.version {
            if let Some((slot, value)) =
                inv.non_utf8_text(&commands::BYTE_FLAGS, &commands::BYTE_ARGS)
            {
                return Err(report(program, false, &non_utf8_argument(&slot, value)));
            }
        }
        for w in inv.warnings.iter().chain(&settings.warnings) {
            brix::log::stderr(&format!("warning: {w}"));
        }
        // A setting flag with a bad value ends the parse, and a usage error
        // ends it too, unless help was asked for before them (`main.cc`
        // ignores usage errors then and shows the help of the command
        // read so far).
        if let Some(e) = &settings.error {
            if !settings.help {
                return Err(report(program, false, &CliError::usage(e.clone())));
            }
        }
        match parse_error {
            Some(error) if !settings.help => return Err(report(program, false, &error.into())),
            Some(_) => {}
            None => {
                for name in &settings.unknown_settings {
                    eprintln!("warning: unknown setting '{name}'");
                }
            }
        }
    }
    if settings.version {
        println!("{}", commands::help::version());
        return Ok(());
    }
    if settings.help {
        match args::render_help(&root, &inv.path, "brix") {
            Some(text) => {
                print!("{text}");
                return Ok(());
            }
            None => {
                return Err(report(
                    program,
                    false,
                    &CliError::usage("no help available"),
                ))
            }
        }
    }
    if inv.path.is_empty() {
        return Err(report(
            program,
            false,
            &CliError::usage("no subcommand specified"),
        ));
    }
    if delegated {
        return Err(report(
            program,
            false,
            &commands::delegate::exec(&inv, &settings, raw_argv),
        ));
    }
    // Once the command is known, and before it runs, `main.cc` looks for
    // an Internet connection; without one it warns and goes on as with
    // `--offline` (see `Ctx::config`).
    if !settings.offline {
        match internet::have_internet() {
            Ok(true) => {}
            Ok(false) => {
                if commands::verbosity(&settings) >= commands::LVL_WARN {
                    brix::log::stderr_bytes(&brix::log::indent_message(
                        "warning",
                        internet::NO_INTERNET_WARNING,
                    ));
                }
                settings.offline = true;
            }
            Err(e) => {
                return Err(report(
                    program,
                    false,
                    &CliError::Brix(brix::Error::Config(e)),
                ))
            }
        }
    }
    // Daemon errors show all their traces with `--show-trace`.
    let show_trace = settings
        .show_trace()
        .expect("settings hold only values their type accepts");
    let ctx = match commands::Ctx::new(&root, &inv, settings) {
        Ok(ctx) => ctx,
        Err(e) => return Err(report(program, show_trace, &e)),
    };
    match commands::dispatch(&ctx) {
        Ok(()) => Ok(()),
        Err(e) => Err(report(program, show_trace, &e)),
    }
}

/// brix's refusal of `value`, given for `slot` (`--flag` or an argument's
/// label), which brix reads as text and which is not UTF-8.
fn non_utf8_argument(slot: &str, value: &[u8]) -> CliError {
    CliError::unsupported(
        [
            format!(
                "brix does not support command-line arguments that are not valid UTF-8 for '{slot}', such as '"
            )
            .as_bytes(),
            value,
            b"'",
        ]
        .concat(),
    )
}

/// Prints an error like `nix` and returns the exit status; `show_trace`
/// is the `show-trace` setting.
fn report(program: &[u8], show_trace: bool, e: &CliError) -> i32 {
    match e {
        CliError::UsageWithHint(msg) => {
            log_error(msg);
            try_help(b"\n", program);
            1
        }
        CliError::Message(msg) => {
            log_error(msg);
            1
        }
        CliError::Brix(e) => {
            brix::log::stderr_bytes(&e.render_bytes(show_trace));
            if let brix::Error::Eval { usage: true, .. } = e {
                try_help(b"", program);
            }
            e.exit_status()
        }
        CliError::Exit(status) => *status,
    }
}

/// `before`, then `Try '<program> --help' for more information.` on
/// stderr. The program is `argv[0]` as given: bytes.
fn try_help(before: &[u8], program: &[u8]) {
    let line = [
        before,
        b"Try '",
        program,
        b" --help' for more information.\n",
    ]
    .concat();
    // As `eprintln!` does, a failure to write to stderr is fatal.
    std::io::stderr()
        .write_all(&line)
        .expect("failed printing to stderr");
}

/// `logError`: `error: <message>` on stderr, its further lines indented
/// under the first (`showErrorInfo`), ANSI escapes only on a terminal.
pub fn log_error(message: impl AsRef<[u8]>) {
    brix::log::stderr_bytes(&brix::log::indent_message("error", message));
}

/// The current directory, absolute: bytes, as Nix's `absPath` has it
/// (`getcwd`), whether or not they are UTF-8.
pub fn cwd() -> Result<BString> {
    let dir = std::env::current_dir()
        .map_err(|e| brix::Error::io("cannot determine the current directory", e))?;
    Ok(dir.into_os_string().into_vec().into())
}
