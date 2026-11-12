//! The commands, one module per `nix` subcommand group, plus what they
//! share: building a session from the settings, resolving installables,
//! and writing output.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::installable::{Installable, Resolver};
use brix::nixconf::NixConf;
use brix::{AutoArg, Config, LockFlags, Session};
use brix_syntax::path::{canon_path, canon_path_bytes};
use bstr::{BStr, BString, ByteSlice};

use crate::args::{Command, Invocation};
use crate::settings::Settings;
use crate::{CliError, Result};

pub mod build;
pub mod bundle;
pub mod delegate;
pub mod derivation;
pub mod develop;
pub mod eval;
pub mod flake;
pub mod formatter;
pub mod hash;
pub mod help;
pub mod path_info;
pub mod realise;
pub mod search;

/// Nix's verbosity levels (`Verbosity`) that commands test.
pub const LVL_WARN: i32 = 1;
pub const LVL_NOTICE: i32 = 2;
pub const LVL_INFO: i32 = 3;
pub const LVL_DEBUG: i32 = 6;

/// The flags whose values brix reads as bytes, as Nix reads every
/// argument ([`crate::args::FlagUse::raw`]): the expressions of `--expr`
/// and `--apply`, and the paths of `--file` and `-I`. A value of any other
/// flag that is not UTF-8 is refused before the command runs (`main`).
pub const BYTE_FLAGS: [&str; 4] = ["expr", "file", "apply", "include"];

/// The positional arguments brix reads as bytes: installables, which can
/// name a store path through any file name ([`Installable::parse`]). As
/// flake references and attribute paths, which brix keeps as text, they
/// are refused when not UTF-8.
pub const BYTE_ARGS: [&str; 2] = ["installable", "installables"];

/// Everything a command needs.
pub struct Ctx<'r> {
    pub root: &'r Command,
    pub inv: &'r Invocation,
    pub settings: Settings,
    /// The current directory, absolute: bytes, as Nix has it. What brix
    /// keeps as text takes it from [`Ctx::cwd_text`] or
    /// [`Ctx::abs_path_text`].
    pub cwd: BString,
}

impl<'r> Ctx<'r> {
    pub fn new(root: &'r Command, inv: &'r Invocation, settings: Settings) -> Result<Ctx<'r>> {
        Ok(Ctx {
            root,
            inv,
            settings,
            cwd: crate::cwd()?,
        })
    }

    /// The current directory as text, for what brix keeps as text: flake
    /// references, lock files and development environment scripts. One
    /// whose name is not UTF-8 is refused there.
    pub fn cwd_text(&self) -> Result<&str> {
        std::str::from_utf8(&self.cwd)
            .map_err(|_| CliError::Brix(crate::installable::cwd_not_utf8(&self.cwd)))
    }

    /// `absPath(path)`: `path` against the current directory unless it is
    /// absolute, made canonical. Bytes, as Nix's paths are.
    pub fn abs_path(&self, path: impl AsRef<[u8]>) -> BString {
        absolute_bytes(&self.cwd, path.as_ref())
    }

    /// [`Ctx::abs_path`] for a path brix keeps as text: the current
    /// directory must be UTF-8 only when `path` is relative.
    pub fn abs_path_text(&self, path: &str) -> Result<String> {
        if path.starts_with('/') {
            return Ok(canon_path(path));
        }
        Ok(absolute(self.cwd_text()?, path))
    }

    /// The command path as text (`"flake show"`).
    pub fn path(&self) -> String {
        self.inv.path.join(" ")
    }

    /// Nix's verbosity: `notice` on a terminal (where the progress bar
    /// shows what `info` would say), `info` otherwise, moved by `-v`/`-q`
    /// (and to `debug` by `--debug`).
    pub fn verbosity(&self) -> i32 {
        verbosity(&self.settings)
    }

    /// `settings.thisSystem`.
    pub fn system(&self) -> String {
        self.settings.system().unwrap_or_else(brix::host_system)
    }

    /// Whether JSON goes out indented (`MixPrintJSON::outputPretty`): the
    /// last of `--pretty` and `--no-pretty`, by default whether stdout is
    /// a terminal.
    pub fn json_pretty(&self) -> bool {
        match self.inv.last_of(&["pretty", "no-pretty"]) {
            Some(flag) => flag.name == "pretty",
            None => std::io::IsTerminal::is_terminal(&std::io::stdout()),
        }
    }

    /// `printJSON`: nlohmann sorts object keys, so brix does too.
    pub fn print_json(&self, value: &serde_json::Value) -> Result<()> {
        print_json(value, self.json_pretty())
    }

    /// `logger->cout("%s", json.dump())`: sorted like [`Ctx::print_json`],
    /// but always on one line, whatever `--pretty` says (`nix build
    /// --json`).
    pub fn print_json_compact(&self, value: &serde_json::Value) -> Result<()> {
        print_json(value, false)
    }

    /// The library configuration the settings describe.
    pub fn config(&self) -> Result<Config> {
        let s = &self.settings;
        if !s.pure_eval()? {
            return Err(CliError::msg(
                "brix only evaluates in pure mode; '--impure' is not supported",
            ));
        }
        let mut config = Config {
            // brix exits as soon as the command is done, so its session
            // leaves the evaluator's memory for the operating system to take
            // back at exit rather than freeing it piece by piece.
            teardown: brix::Teardown::Leak,
            ..Config::default()
        };
        if let Some(dir) = store_dir_from_env()? {
            config.eval.store_dir = dir;
        }
        config.eval.show_trace = s.show_trace()?;
        config.eval.experimental_features = s.experimental_features();
        config.eval.lints = s.lints()?;
        if let Some(n) = s.get_u32("max-call-depth")? {
            config.eval.max_call_depth = n;
        }
        if let Some(n) = s.get_u32("eval-cores")? {
            config.eval.eval_cores = n;
        }
        config.eval.allow_import_from_derivation =
            s.get_bool("allow-import-from-derivation", true)?;
        // The store, the settings forwarded to its daemon and the fetch
        // settings come from nix.conf (system file, user files, NIX_CONFIG)
        // with the command line on top, as in Nix: brix must use the same
        // store, configured the same way, as the `nix` that runs the
        // commands brix hands over (`delegate`). Evaluation settings
        // deliberately stay command-line only.
        let conf = self.nix_conf()?;
        // This build of brix runs the commands that evaluate on several
        // threads (`eval_workers`), with as many as that says.
        #[cfg(brix_parallel)]
        {
            config.eval.eval_cores = eval_workers(&conf)?;
        }
        let store = store_config(&conf)?;
        config.daemon = store.daemon;
        config.daemon_settings = store.settings;
        if let Some(dir) = store.store_dir {
            config.eval.store_dir = dir;
        }
        config.build_log = if s.print_build_logs {
            brix::BuildLogMode::Logs
        } else if s.verbosity < 0 {
            brix::BuildLogMode::Quiet
        } else {
            brix::BuildLogMode::Normal
        };
        config.fetch.notices = self.verbosity() >= LVL_NOTICE;
        config.fetch.infos = self.verbosity() >= LVL_INFO;
        config.fetch.access_tokens = conf.access_tokens();
        config.fetch.netrc_file = Some(conf.netrc_file());
        // `Setting<std::optional<AbsolutePath>>`: empty is none, and a
        // relative path is refused. Unset, it keeps Nix's default.
        match conf.get("ssl-cert-file") {
            None => {}
            Some("") => config.fetch.ssl_cert_file = None,
            Some(file) if file.starts_with('/') => {
                // A setting is text, so the message is too.
                let canon =
                    brix_store::CanonPath::new(file).map_err(|e| CliError::msg(e.to_string()))?;
                config.fetch.ssl_cert_file = Some(os_path(canon.into_bytes()));
            }
            Some(file) => {
                return Err(CliError::msg(format!("not an absolute path: \"{file}\"")));
            }
        }
        if let Some(registry) = conf.get("flake-registry") {
            config.fetch.flake_registry = registry.to_owned();
        }
        if let Some(ttl) = conf.get_u32("tarball-ttl")? {
            config.fetch.tarball_ttl = ttl.into();
        }
        if let Some(n) = conf.get_u64("http-connections")? {
            config.fetch.http_connections = usize::try_from(n).unwrap_or(usize::MAX);
        }
        // `nix` draws its progress bar on a terminal unless another log
        // format is asked for.
        let bar_format = matches!(
            self.inv.flag_value("log-format"),
            None | Some("bar" | "bar-with-logs")
        );
        if bar_format && brix::log::is_tty() && s.verbosity >= 0 {
            let bar = crate::progress::progress_bar();
            config.progress = bar.clone();
            config.build_progress = Some(bar);
        }
        if let Some(allow) = conf.get_bool("allow-dirty").map_err(CliError::Brix)? {
            config.fetch.allow_dirty = allow;
        }
        if let Some(warn) = conf.get_bool("warn-dirty").map_err(CliError::Brix)? {
            config.fetch.warn_dirty = warn;
        }
        if let Some(allow) = conf.get_bool("allow-dirty-locks").map_err(CliError::Brix)? {
            config.fetch.allow_dirty_locks = allow;
        }
        if let Some(trust) = conf
            .get_bool("trust-tarballs-from-git-forges")
            .map_err(CliError::Brix)?
        {
            config.fetch.trust_tarballs_from_git_forges = trust;
        }
        if let Some(compat) = conf.get_bool("nix-219-compat").map_err(CliError::Brix)? {
            config.fetch.nix219_compat = compat;
        }
        // Set as `download-attempts` too, its alias.
        if let Some(n) = conf
            .get_u32("filetransfer-retry-attempts")
            .map_err(CliError::Brix)?
        {
            config.fetch.download_attempts = n;
        }
        if let Some(ms) = conf
            .get_u32("filetransfer-retry-delay")
            .map_err(CliError::Brix)?
        {
            config.fetch.download_retry_delay_ms = ms;
        }
        // `unsigned long` seconds; 0 is no limit.
        if let Some(secs) = conf.get_u64("connect-timeout").map_err(CliError::Brix)? {
            config.fetch.connect_timeout = secs;
        }
        // `--offline`, or no Internet connection (see `main`). `--refresh`
        // below wins over it, as in `main.cc`.
        if s.offline {
            disable_net(&conf, &mut config);
        }
        // brix's own switches, which the library takes as settings.
        config.eval.timings = env_switch("BRIX_TIMINGS")?;
        config.gc.trace_collections = config.eval.timings;
        if let Some(workers) = std::env::var_os("BRIX_GC_WORKERS") {
            config.gc.workers = workers.to_str().and_then(|value| value.parse().ok())
                .filter(|&value| value <= 64)
                .ok_or_else(|| CliError::Brix(brix::Error::Config(
                    "BRIX_GC_WORKERS must be between 0 (automatic) and 64".into(),
                )))?;
        }
        if let Some(bytes) = std::env::var_os("BRIX_GC_THRESHOLD_BYTES") {
            config.gc.threshold_bytes = bytes
                .to_str()
                .and_then(|bytes| bytes.parse::<usize>().ok())
                .filter(|&bytes| bytes > 0)
                .ok_or_else(|| {
                    CliError::Brix(brix::Error::Config(
                        "BRIX_GC_THRESHOLD_BYTES must be a positive number of bytes".into(),
                    ))
                })?;
        }
        // A single override keeps the previous fixed-threshold semantics.
        if std::env::var_os("BRIX_GC_THRESHOLD_BYTES").is_some() {
            config.gc.steady_threshold_bytes = None;
        }
        if let Some(bytes) = std::env::var_os("BRIX_GC_STEADY_THRESHOLD_BYTES") {
            config.gc.steady_threshold_bytes = Some(
                bytes
                    .to_str()
                    .and_then(|bytes| bytes.parse::<usize>().ok())
                    .filter(|&bytes| bytes > 0)
                    .ok_or_else(|| {
                        CliError::Brix(brix::Error::Config(
                            "BRIX_GC_STEADY_THRESHOLD_BYTES must be a positive number of bytes"
                                .into(),
                        ))
                    })?,
            );
        }
        config.fetch.verify_cache = env_switch("BRIX_FETCH_CACHE_VERIFY")?;
        // Compiled flake-input images (brix extensions).
        if self.inv.has("no-compiled-inputs") {
            config.compiled.enabled = false;
        }
        for spec in self.inv.flag_uses("compiled-input") {
            let Some(spec) = spec.first() else {
                continue;
            };
            // The last '=': SRI hashes end in '=' padding; paths rarely contain one.
            let Some((key, path)) = spec.rsplit_once('=') else {
                return Err(CliError::usage(format!(
                    "'--compiled-input' expects NAME=PATH or NARHASH=PATH, not '{spec}'"
                )));
            };
            config
                .compiled
                .explicit
                .push((key.to_owned(), os_path(self.abs_path(path))));
        }
        if s.refresh {
            config.fetch.tarball_ttl = 0;
        }
        // `--file` reads an expression outside the store: its directory
        // must be readable (Nix's `--file` implies `--impure`). Its name
        // is bytes.
        if let Some(file) = self.inv.flag_value_bytes("file") {
            if file != "-" {
                let abs = os_path(self.abs_path(file));
                let dir = abs
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), |p| p.to_path_buf());
                config.allowed_paths.push(dir);
            }
        }
        for path in self.inv.flag_uses_bytes("include") {
            // `-I path` entries are lookup paths, which pure evaluation has
            // none of; the directory is made readable so that `--file`
            // expressions can still use it. Bytes, as `--file`'s.
            if let Some(p) = path.first() {
                let p = match p.find_byte(b'=') {
                    Some(i) => &p[i + 1..],
                    None => &p[..],
                };
                config.allowed_paths.push(os_path(self.abs_path(p)));
            }
        }
        Ok(config)
    }

    /// The configuration files as Nix reads them, with this command line's
    /// settings applied on top.
    pub(crate) fn nix_conf(&self) -> Result<NixConf> {
        // What Nix rejects in its configuration files is a `UsageError`.
        let mut conf = NixConf::load().map_err(|e| match e {
            brix::Error::Usage(message) => CliError::UsageWithHint(message),
            other => other.into(),
        })?;
        self.settings.apply_to(&mut conf);
        Ok(conf)
    }

    pub fn session(&self) -> Result<Session> {
        Ok(Session::new(self.config()?)?)
    }

    /// A session that takes no earlier download to be current
    /// (`tarball-ttl` 0), whatever the configuration and `--offline` say:
    /// `nix flake lock` and `nix flake update` set
    /// `fetchSettings.tarballTtl = 0` when they run (`CmdFlakeLock::run`,
    /// `CmdFlakeUpdate::run`).
    pub fn refetching_session(&self) -> Result<Session> {
        let mut config = self.config()?;
        config.fetch.tarball_ttl = 0;
        Ok(Session::new(config)?)
    }

    /// `MixFlakeOptions`: the lock flags the flake-related options select,
    /// from Nix's defaults: a lock file that needs changes is updated and
    /// written, unless the options say otherwise. Flake references are
    /// parsed with `session`'s fetch settings.
    pub fn lock_flags(&self, session: &Session) -> Result<LockFlags> {
        self.lock_flags_with(session, None)
    }

    /// [`Ctx::lock_flags`], with the inputs `--update-input` names already
    /// read when given (see [`Ctx::update_input_flags`]).
    pub fn lock_flags_with(
        &self,
        session: &Session,
        input_updates: Option<BTreeSet<brix_flake::InputAttrPath>>,
    ) -> Result<LockFlags> {
        let mut flags = LockFlags::default();
        if self.inv.has("recreate-lock-file") {
            eprintln!("warning: '--recreate-lock-file' is deprecated and will be removed in a future version; use 'brix flake update' instead.");
            flags.recreate_lock_file = true;
        }
        if self.inv.has("no-update-lock-file") {
            flags.update_lock_file = false;
        }
        if self.inv.has("no-write-lock-file") {
            flags.write_lock_file = false;
        }
        // `use-registries` defaults to true; `nix`'s pure mode still
        // resolves indirect references through the registries.
        flags.use_registries = self.settings.get_bool("use-registries", true)?;
        if self.inv.has("no-registries") {
            eprintln!("warning: '--no-registries' is deprecated; use '--no-use-registries'");
            flags.use_registries = false;
        }
        flags.input_updates = match input_updates {
            Some(input_updates) => input_updates,
            None => self.update_input_flags()?,
        };
        for v in self.inv.flag_uses("override-input") {
            flags.write_lock_file = false;
            let path =
                brix_flake::lockfile::parse_input_attr_path(&v[0]).map_err(brix::Error::Flake)?;
            if path.is_empty() {
                return Err(CliError::usage("--override-input was passed a zero-length input path, which would refer to the flake itself, not an input"));
            }
            let opts = brix_flake::ParseOptions {
                base_dir: Some(self.cwd_text()?.to_owned()),
                allow_missing: true,
                ..brix_flake::ParseOptions::flake()
            };
            let flake_ref = brix_flake::parse_flake_ref(&session.fetch_settings(), &v[1], &opts)
                .map_err(brix::Error::Flake)?;
            flags.input_overrides.insert(path, flake_ref);
        }
        if let Some(path) = self.inv.flag_value("output-lock-file") {
            flags.output_lock_file = Some(self.abs_path_text(path)?);
        }
        if let Some(path) = self.inv.flag_value("reference-lock-file") {
            // `readLockFile`: a lock file that does not exist is an empty
            // one.
            let path = self.abs_path_text(path)?;
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    brix_flake::LockFile::new().to_json_string()
                }
                // `readFile`.
                Err(e) => return Err(brix::Error::io(format!("opening file \"{path}\""), e).into()),
            };
            flags.reference_lock_file = Some(text);
        }
        if self.inv.has("commit-lock-file") {
            return Err(unsupported_flag("--commit-lock-file"));
        }
        if self.inv.has("inputs-from") {
            return Err(unsupported_flag("--inputs-from"));
        }
        if self.inv.has("override-flake") {
            return Err(unsupported_flag("--override-flake"));
        }
        Ok(flags)
    }

    /// The inputs `--update-input` (deprecated) names, each with Nix's
    /// warning, in the order given. Its handler does not look for one
    /// named twice; `flake update` does, for its positionals.
    pub fn update_input_flags(&self) -> Result<BTreeSet<brix_flake::InputAttrPath>> {
        let mut input_updates = BTreeSet::new();
        for v in self.inv.flag_uses("update-input") {
            eprintln!("warning: '--update-input' is a deprecated alias for 'flake update' and will be removed in a future version.");
            let path =
                brix_flake::lockfile::parse_input_attr_path(&v[0]).map_err(brix::Error::Flake)?;
            if path.is_empty() {
                return Err(CliError::usage("--update-input was passed a zero-length input path, which would refer to the flake itself, not an input"));
            }
            input_updates.insert(path);
        }
        Ok(input_updates)
    }

    /// `--arg`, `--argstr`, `--arg-from-file`, `--arg-from-stdin`.
    pub fn auto_args(&self) -> Result<Vec<(String, AutoArg)>> {
        let mut args: BTreeMap<String, AutoArg> = BTreeMap::new();
        for f in &self.inv.flags {
            match f.name.as_str() {
                "arg" => {
                    args.insert(f.values[0].clone(), AutoArg::Expr(f.values[1].clone()));
                }
                "argstr" => {
                    args.insert(f.values[0].clone(), AutoArg::String(f.values[1].clone()));
                }
                "arg-from-file" => {
                    // `readFile` of the path as given.
                    let path = &f.values[1];
                    let text = std::fs::read_to_string(path).map_err(|e| {
                        CliError::msg(format!(
                            "opening file \"{path}\": {}",
                            realise::strerror(&e)
                        ))
                    })?;
                    args.insert(f.values[0].clone(), AutoArg::String(text));
                }
                "arg-from-stdin" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    args.insert(f.values[0].clone(), AutoArg::String(text));
                }
                _ => {}
            }
        }
        Ok(args.into_iter().collect())
    }

    /// `--default-flake-schemas`: the flake whose schemas apply to flakes
    /// that bring none.
    pub fn default_schemas(&self, session: &Session) -> Result<Option<brix::FlakeRef>> {
        match self.inv.flag_value("default-flake-schemas") {
            Some(s) => Ok(Some(session.parse_flake_ref(s, self.cwd_text()?)?)),
            None => Ok(None),
        }
    }

    /// A resolver for flakes the command names itself (a bundler, a
    /// template, nixpkgs' bash), locked with `lock_flags`, with the roles
    /// set and `--default-flake-schemas` applied.
    pub fn resolver_with<'s>(
        &self,
        session: &'s Session,
        lock_flags: LockFlags,
        roles: &[&'static str],
    ) -> Result<Resolver<'s>> {
        Ok(Resolver::new(
            session,
            &self.cwd,
            &self.system(),
            roles.iter().copied(),
            lock_flags,
        )
        .with_default_schemas(self.default_schemas(session)?))
    }

    /// A resolver for this command's installables: `--file`/`--expr`
    /// evaluated (`SourceExprCommand::parseInstallables`: the expression's
    /// bytes, or the file named by bytes), the lock flags applied, the
    /// roles set.
    pub fn resolver<'s>(
        &self,
        session: &'s Session,
        roles: &[&'static str],
    ) -> Result<Resolver<'s>> {
        let file = self.inv.flag_value_bytes("file");
        let expr = self.inv.flag_value_bytes("expr");
        if file.is_some() && expr.is_some() {
            return Err(CliError::usage("'--file' and '--expr' are exclusive"));
        }
        // Read once: `--arg-from-stdin` consumes standard input.
        let auto_args = self.auto_args()?;
        let flake_with_args = file.is_none() && expr.is_none() && !auto_args.is_empty();
        let mut resolver = self
            .resolver_with(session, self.lock_flags(session)?, roles)?
            .with_auto_args(auto_args);
        if let Some(expr) = expr {
            resolver = resolver.with_expr(expr)?;
        } else if let Some(file) = file {
            if file == "-" {
                // `parseStdin`: the bytes of standard input.
                let mut source = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut source)?;
                resolver = resolver.with_expr(&source)?;
            } else {
                resolver = resolver.with_file(self.abs_path(file))?;
            }
        } else if flake_with_args {
            return Err(CliError::usage(
                "'--arg' and '--argstr' are incompatible with flakes",
            ));
        }
        Ok(resolver)
    }

    /// The `installables` argument, with Nix's default of `.` when none
    /// were given (`applyDefaultInstallables`); `--stdin` adds the words
    /// of standard input (`std::cin >> word`: between white space of the C
    /// locale, bytes) instead of the default.
    pub fn installables(&self, resolver: &Resolver<'_>) -> Result<Vec<Installable>> {
        let mut raw: Vec<BString> = self.inv.arg_bytes("installables").to_vec();
        if self.inv.has("stdin") && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            let mut input = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut input)?;
            raw.extend(
                input
                    .split(|b| b" \t\n\x0b\x0c\r".contains(b))
                    .filter(|word| !word.is_empty())
                    .map(BString::from),
            );
        } else if raw.is_empty() {
            raw.push(".".into());
        }
        Ok(resolver.parse(&raw)?)
    }

    /// The single `installable` argument (default `.`).
    pub fn installable(&self, resolver: &Resolver<'_>) -> Result<Installable> {
        let raw: &BStr = self
            .inv
            .arg_one_bytes("installable")
            .unwrap_or(BStr::new("."));
        let mut v = resolver.parse(&[raw])?;
        Ok(v.remove(0))
    }

    /// The `flake-url` argument of the `flake` commands (default `.`).
    pub fn flake_ref(&self, session: &Session) -> Result<brix::FlakeRef> {
        let raw = self
            .inv
            .flag_value("flake")
            .or_else(|| self.inv.arg_one("flake-url"))
            .unwrap_or(".");
        Ok(session.parse_flake_ref(raw, self.cwd_text()?)?)
    }
}

/// The settings `RemoteStore::setOptions` sends in fixed fields, with
/// their effective values, wherever they were set.
const DAEMON_FIELDS: [&str; 7] = [
    "max-jobs",
    "keep-going",
    "keep-failed",
    "cores",
    "fallback",
    "substitute",
    "max-silent-time",
];

/// The other settings a `nix` client forwards to its daemon that brix
/// forwards too: only when overridden (set after the system file, which
/// the daemon reads itself).
const DAEMON_OVERRIDES: [&str; 6] = [
    "sandbox",
    "timeout",
    "builders",
    "substituters",
    "trusted-public-keys",
    "require-sigs",
];

/// The store directory the environment sets, as `StoreDirSetting`'s
/// default reads it: the first non-empty of `NIX_STORE_DIR` and
/// `NIX_STORE`, made canonical by `canonStoreDir` (a relative one is a
/// usage error). brix keeps the directory as text, so it must be UTF-8.
fn store_dir_from_env() -> Result<Option<brix::StoreDir>> {
    for var in ["NIX_STORE_DIR", "NIX_STORE"] {
        let Some(value) = std::env::var_os(var).filter(|v| !v.is_empty()) else {
            continue;
        };
        let value = value.to_str().ok_or_else(|| {
            CliError::Brix(brix::Error::Config(format!(
                "the store directory in {var} is not valid UTF-8"
            )))
        })?;
        return brix::StoreDir::parse_setting(value)
            .map(Some)
            .map_err(|e| CliError::UsageWithHint(e.to_string().into()));
    }
    Ok(None)
}

/// Nix's verbosity for `settings`: `notice` on a terminal (where the
/// progress bar shows what `info` would say), `info` otherwise, moved by
/// `-v`/`-q` (and to `debug` by `--debug`).
pub fn verbosity(settings: &Settings) -> i32 {
    let base = if brix::log::is_tty() {
        LVL_NOTICE
    } else {
        LVL_INFO
    };
    base + settings.verbosity
}

/// How many threads a command that can evaluate on several uses: Nix's
/// `eval-cores` setting, from `nix.conf` or the command line
/// (`--eval-cores N`, `--option eval-cores N`). Unset or 0, which Nix
/// reads as "every core", it is the number of hardware threads on the
/// machine's performance cores: more workers than that are slower.
pub(crate) fn eval_workers(conf: &NixConf) -> brix::Result<u32> {
    Ok(match conf.get_u32("eval-cores")? {
        None | Some(0) => brix::performance_threads().try_into().unwrap_or(u32::MAX),
        Some(cores) => cores,
    })
}

/// `disableNet` (`src/nix/main.cc`), for `--offline` or a machine without
/// an Internet connection: each network setting the user has not set
/// (in a user `nix.conf`, `NIX_CONFIG` or on the command line; the system
/// `nix.conf` does not count, see [`NixConf::is_overridden`]) changes to
/// suit working offline. The daemon is told not to substitute; every
/// earlier download stays current (`tarball-ttl` at `unsigned int`'s
/// maximum), though what was never fetched is still downloaded; a
/// download is tried once (`filetransfer-retry-attempts` 0); and a
/// connection gets a second (`connect-timeout` 1). (Nix also stops
/// expiring its binary-cache metadata, `narinfo-cache-meta-ttl`, which
/// brix, leaving substitution to the daemon, does not keep.)
fn disable_net(conf: &NixConf, config: &mut Config) {
    if !conf.is_overridden("substitute") {
        config
            .daemon_settings
            .insert("substitute".to_owned(), "false".to_owned());
    }
    if !conf.is_overridden("tarball-ttl") {
        config.fetch.tarball_ttl = u32::MAX.into();
    }
    if !conf.is_overridden("filetransfer-retry-attempts") {
        config.fetch.download_attempts = 0;
    }
    if !conf.is_overridden("connect-timeout") {
        config.fetch.connect_timeout = 1;
    }
}

/// A switch brix reads from the environment: `1` turns it on, `0` (or
/// leaving it unset) off; any other value is a configuration error rather
/// than a silent "off".
fn env_switch(var: &str) -> Result<bool> {
    match std::env::var_os(var) {
        None => Ok(false),
        Some(value) if value == "1" => Ok(true),
        Some(value) if value == "0" => Ok(false),
        Some(value) => Err(CliError::Brix(brix::Error::Config(format!(
            "{var} must be 1 or 0, not '{}'",
            value.to_string_lossy()
        )))),
    }
}

/// The store the configuration names, as the library takes it.
#[derive(Debug, PartialEq, Eq)]
struct StoreConfig {
    /// The daemon's URI; `None` for Nix's dummy store (`dummy://`).
    daemon: Option<String>,
    /// The settings to forward to the daemon.
    settings: BTreeMap<String, String>,
    /// The store directory the store URI sets (`?store=`), if it does.
    store_dir: Option<brix::StoreDir>,
}

/// The store `conf` names and the settings to forward to its daemon.
/// Nix's default store, `auto`, is the daemon for a user who cannot write
/// the store; brix never opens a store itself, so by `auto` and `local`
/// it always means the daemon.
fn store_config(conf: &NixConf) -> Result<StoreConfig> {
    let (daemon, store_dir) = match conf.get("store").unwrap_or("auto") {
        "" | "auto" | "local" => (Some("daemon".to_owned()), None),
        uri => match uri.strip_prefix("dummy:") {
            Some(rest) => (None, dummy_store_dir(uri, rest)?),
            None => (Some(uri.to_owned()), daemon_store_dir(uri)?),
        },
    };
    let fields = DAEMON_FIELDS
        .iter()
        .filter_map(|&name| conf.get(name).map(|v| (name, v)));
    let overrides = DAEMON_OVERRIDES
        .iter()
        .filter(|&&name| conf.is_overridden(name))
        .filter_map(|&name| conf.get(name).map(|v| (name, v)));
    let settings = fields
        .chain(overrides)
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    Ok(StoreConfig {
        daemon,
        settings,
        store_dir,
    })
}

/// The store directory a daemon's URI sets (`?store=`): as for
/// `dummy://`, the store's `storeDir` is the one evaluation uses. The
/// URI's warnings are given when the session opens the store, which
/// parses it again.
fn daemon_store_dir(uri: &str) -> Result<Option<brix::StoreDir>> {
    let parsed = brix_daemon::StoreUri::parse(uri, &|_| {}).map_err(|e| match e {
        brix_daemon::DaemonError::Usage(msg) => CliError::usage(msg),
        e => CliError::Brix(brix::Error::Daemon(e)),
    })?;
    Ok(parsed.store_dir.map(|dir| {
        brix::StoreDir::parse_setting(&dir).expect("invariant: the URI parser canonicalised it")
    }))
}

/// `DummyStoreConfig` from the URI `uri`, `rest` being what follows
/// `dummy:` (and `//`, if there): no authority, then settings of which
/// only `store` (the store directory) matters to brix. The others are checked as Nix
/// checks them, and a setting the dummy store does not have is warned
/// about.
fn dummy_store_dir(uri: &str, rest: &str) -> Result<Option<brix::StoreDir>> {
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !authority.is_empty() {
        return Err(CliError::usage(format!(
            "`dummy` store URIs must not contain an authority part {authority}"
        )));
    }
    // `decodeQuery(lenient)`: a setting without `=` is warned about and
    // ignored.
    let params = brix_store::url::decode_query(query, true, &|msg| {
        brix::log::stderr_bytes(&brix::log::indent_message("warning", msg))
    })
    .map_err(|_| CliError::usage(format!("Cannot parse Nix store '{uri}'")))?;
    let mut store_dir = None;
    for (name, value) in &params {
        let checked = match name.as_str() {
            "store" => brix::StoreDir::parse_setting(value)
                .map(|dir| store_dir = Some(dir))
                .map_err(|e| e.to_string()),
            "read-only" | "trusted" | "want-mass-query" => brix::nixconf::parse_bool(name, value)
                .map(drop)
                .map_err(|e| e.to_string()),
            "priority" | "path-info-cache-size" => brix::nixconf::parse_int::<i32>(name, value)
                .map(drop)
                .map_err(|e| e.to_string()),
            "system-features" => Ok(()),
            _ => {
                brix::log::stderr(&format!("warning: unknown setting '{name}'"));
                Ok(())
            }
        };
        checked.map_err(CliError::usage)?;
    }
    Ok(store_dir)
}

/// What runs a command brix implements.
type Handler = fn(&Ctx<'_>) -> Result<()>;

/// The command brix implements at `path` (`"flake show"`), if it does.
fn handler(path: &str) -> Option<Handler> {
    let handler: Handler = match path {
        "eval" => eval::run,
        "build" => build::build,
        "run" => build::run,
        "env shell" => build::shell,
        "fmt" | "formatter run" => formatter::run,
        "formatter build" => formatter::build,
        "bundle" => bundle::run,
        "path-info" => path_info::run,
        "search" => search::run,
        "flake metadata" => |ctx| flake::metadata(ctx, false),
        "flake info" => |ctx| flake::metadata(ctx, true),
        "flake show" => flake::show,
        "flake lock" => flake::lock,
        "flake update" => flake::update,
        "flake check" => flake::check,
        "flake prefetch" => flake::prefetch,
        "flake prefetch-inputs" => flake::prefetch_inputs,
        "flake compile" => flake::compile,
        "flake init" => |ctx| flake::init(ctx, false),
        "flake new" => |ctx| flake::init(ctx, true),
        "derivation show" => derivation::show,
        "derivation add" => derivation::add,
        "develop" => develop::develop,
        "print-dev-env" => develop::print_dev_env,
        "hash file" => hash::file,
        "hash path" => hash::path,
        "hash convert" => hash::convert,
        "hash to-base16" => |ctx| hash::to_base(ctx, brix_store::HashFormat::Base16),
        "hash to-base32" => |ctx| hash::to_base(ctx, brix_store::HashFormat::Nix32),
        "hash to-base64" => |ctx| hash::to_base(ctx, brix_store::HashFormat::Base64),
        "hash to-sri" => |ctx| hash::to_base(ctx, brix_store::HashFormat::Sri),
        "help" => help::help,
        "help-stores" => help::help_stores,
        "__dump-cli" => |ctx| print_stdout(&format!("{}\n", help::dump_cli(ctx.root))),
        "__parse" => help::parse,
        _ => return None,
    };
    Some(handler)
}

/// Whether the command at `path` is one of `nix`'s that brix does not
/// implement, and so hands to Nix ([`delegate`]). A group without its
/// subcommand (`brix flake`) is not: brix reports it.
pub fn delegated(root: &Command, path: &[String]) -> bool {
    !path.is_empty()
        && handler(&path.join(" ")).is_none()
        && root.descend(path).is_some_and(|c| c.commands.is_empty())
}

/// Runs the selected command, one brix implements or a group without its
/// subcommand; the caller hands the [`delegated`] ones to Nix.
pub fn dispatch(ctx: &Ctx<'_>) -> Result<()> {
    let path = ctx.path();
    if let Some(run) = handler(&path) {
        return run(ctx);
    }
    // A group without a subcommand (`NixMultiCommand::run`).
    let cmd = ctx
        .root
        .descend(&ctx.inv.path)
        .expect("the parser selected an existing command");
    assert!(
        !cmd.commands.is_empty(),
        "'{path}' is delegated to Nix, not dispatched"
    );
    // Nix sorts the Markdown items (`- \`name\``), so `prefetch-inputs`
    // comes before `prefetch`.
    let mut names: Vec<&str> = cmd
        .commands
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| c.name.as_str())
        .collect();
    names.sort_by_key(|name| format!("{name}`"));
    let lines: Vec<String> = names.iter().map(|name| format!("  · {name}")).collect();
    Err(CliError::usage(format!(
        "brix {path} requires a sub-command. Available sub-commands:\n\n{}",
        lines.join("\n")
    )))
}

/// Writes to stdout, treating a closed pipe as success.
pub fn print_stdout(s: &str) -> Result<()> {
    print_stdout_bytes(s.as_bytes())
}

/// [`print_stdout`] for output that holds Nix strings, which need not be
/// UTF-8: the bytes are written as they are.
pub fn print_stdout_bytes(s: &[u8]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match lock.write_all(s).and_then(|()| lock.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `absPath`: `path` relative to `base` unless already absolute, made
/// canonical (`canonPath` without resolving symlinks: no `.`, `..`,
/// repeated or trailing `/`). For text; see [`absolute_bytes`].
pub fn absolute(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        canon_path(path)
    } else {
        canon_path(&format!("{base}/{path}"))
    }
}

/// [`absolute`] on bytes, as Nix's paths are.
pub fn absolute_bytes(base: &[u8], path: &[u8]) -> BString {
    if path.first() == Some(&b'/') {
        canon_path_bytes(path).into()
    } else {
        canon_path_bytes(&[base, b"/", path].concat()).into()
    }
}

/// The file-system path whose name is `bytes` (brix's command line runs
/// on Unix, where any bytes name a file).
pub fn os_path(bytes: impl Into<Vec<u8>>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.into()))
}

fn unsupported_flag(flag: &str) -> CliError {
    CliError::msg(format!("'{flag}' is not supported by brix yet"))
}

fn print_json(value: &serde_json::Value, pretty: bool) -> Result<()> {
    let sorted = sort_json(value);
    print_stdout(&format!(
        "{}\n",
        crate::output::json_to_string(&sorted, pretty)
    ))
}

/// Object keys in sorted order, recursively (nlohmann's `json::object`
/// is a `std::map`).
pub fn sort_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(m) => {
            let sorted: BTreeMap<&String, serde_json::Value> =
                m.iter().map(|(k, v)| (k, sort_json(v))).collect();
            serde_json::Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sort_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{args, tree};

    /// The daemon and the settings forwarded to it.
    fn daemon_config(conf: &NixConf) -> (Option<String>, BTreeMap<String, String>) {
        let store = store_config(conf).unwrap();
        (store.daemon, store.settings)
    }

    /// The store and daemon settings for nix.conf `text` with the command
    /// line `argv` on top.
    fn daemon(text: &str, argv: &str) -> (Option<String>, BTreeMap<String, String>) {
        let root = tree::build();
        let argv: Vec<String> = argv.split_whitespace().map(str::to_owned).collect();
        let inv = args::parse(&root, &tree::aliases(), &argv).unwrap();
        let mut conf = NixConf::default();
        conf.apply_text(text, Path::new("nix.conf")).unwrap();
        Settings::from_invocation(&root, &inv).apply_to(&mut conf);
        daemon_config(&conf)
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const CONF: &str =
        "store = unix:///run/x.sock\nmax-jobs = 4\nsubstituters = a\nwarn-dirty = false\n";

    #[test]
    fn store_and_daemon_settings_come_from_nix_conf() {
        assert_eq!(
            daemon(CONF, "eval --expr 1"),
            (
                Some("unix:///run/x.sock".to_owned()),
                map(&[("max-jobs", "4"), ("substituters", "a")])
            )
        );
    }

    #[test]
    fn the_command_line_wins_and_extra_appends() {
        assert_eq!(
            daemon(
                CONF,
                "--store dummy:// --max-jobs 2 --extra-substituters b eval --expr 1"
            ),
            (None, map(&[("max-jobs", "2"), ("substituters", "a b")]))
        );
    }

    /// The daemon reads the system file itself: of its settings, only
    /// those `setOptions` sends in fixed fields are forwarded.
    #[test]
    fn system_file_settings_reach_the_daemon_only_in_fixed_fields() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("nix.conf");
        std::fs::write(&system, format!("{CONF}sandbox = false\n")).unwrap();
        let user = dir.path().join("user.conf");
        std::fs::write(&user, "extra-substituters = b\n").unwrap();
        let conf = |user_files: Vec<std::path::PathBuf>| {
            brix::nixconf::NixConf::load_from(&brix::nixconf::Sources {
                system_file: Some(system.clone()),
                user_files,
                env_text: None,
                nix_remote: None,
            })
            .unwrap()
        };
        assert_eq!(
            daemon_config(&conf(Vec::new())),
            (
                Some("unix:///run/x.sock".to_owned()),
                map(&[("max-jobs", "4")])
            )
        );
        assert_eq!(
            daemon_config(&conf(vec![user])).1,
            map(&[("max-jobs", "4"), ("substituters", "a b")])
        );
    }

    #[test]
    fn the_default_store_is_the_daemon() {
        assert_eq!(
            daemon("", "eval --expr 1"),
            (Some("daemon".to_owned()), BTreeMap::new())
        );
        assert_eq!(
            daemon("store = auto\n", "eval --expr 1").0.as_deref(),
            Some("daemon")
        );
    }

    #[test]
    fn absolute_paths_are_canonical() {
        assert_eq!(absolute("/base", "x"), "/base/x");
        assert_eq!(absolute("/base", "./"), "/base");
        assert_eq!(absolute("/base", "."), "/base");
        assert_eq!(absolute("/base", "sub/../x//y/"), "/base/x/y");
        assert_eq!(absolute("/base/", "../x"), "/x");
        assert_eq!(absolute("/base", "/a/./c/"), "/a/c");
    }

    /// `absPath` on bytes: names that are not UTF-8 are components like
    /// any other.
    #[test]
    fn absolute_paths_are_bytes() {
        assert_eq!(absolute_bytes(b"/d\xe9", b"x"), &b"/d\xe9/x"[..]);
        assert_eq!(
            absolute_bytes(b"/d\xe9", b"./caf\xe9/"),
            &b"/d\xe9/caf\xe9"[..]
        );
        assert_eq!(absolute_bytes(b"/d\xe9", b".."), &b"/"[..]);
        assert_eq!(absolute_bytes(b"/d\xe9", b"\xff/../y"), &b"/d\xe9/y"[..]);
        assert_eq!(absolute_bytes(b"/d\xe9", b"/a\xff/./b"), &b"/a\xff/b"[..]);
    }

    /// The current directory is bytes; what brix keeps as text needs it as
    /// text only to resolve a relative path, and refuses one that is not
    /// UTF-8 by name.
    #[test]
    fn a_current_directory_that_is_not_utf8() {
        let root = tree::build();
        let inv = args::parse(&root, &tree::aliases(), &["eval", "--expr", "1"]).unwrap();
        let ctx = Ctx {
            root: &root,
            inv: &inv,
            settings: Settings::from_invocation(&root, &inv),
            cwd: BString::from(&b"/tmp/d\xe9"[..]),
        };
        assert_eq!(ctx.abs_path("x"), &b"/tmp/d\xe9/x"[..]);
        assert_eq!(ctx.abs_path_text("/etc/./x").unwrap(), "/etc/x");
        for refused in [ctx.cwd_text().map(str::to_owned), ctx.abs_path_text("x")] {
            let message = match refused {
                Err(CliError::Brix(brix::Error::Usage(message))) => message,
                other => panic!("not refused: {other:?}"),
            };
            assert_eq!(
                message,
                &b"brix does not support flake references or file arguments in current directories that are not valid UTF-8, such as '/tmp/d\xe9'"[..]
            );
        }
        let ctx = Ctx {
            cwd: BString::from("/tmp/d"),
            ..ctx
        };
        assert_eq!(ctx.cwd_text().unwrap(), "/tmp/d");
        assert_eq!(ctx.abs_path_text("x/../y").unwrap(), "/tmp/d/y");
    }

    #[test]
    fn delegated_commands() {
        let root = tree::build();
        let path = |p: &str| -> Vec<String> { p.split(' ').map(str::to_owned).collect() };
        for p in ["log", "store gc", "profile list", "repl", "why-depends"] {
            assert!(delegated(&root, &path(p)), "{p}");
        }
        // brix's own commands, a group without its subcommand, and no
        // command at all are not.
        for p in [
            "eval",
            "flake show",
            "flake compile",
            "hash to-sri",
            "__dump-cli",
            "flake",
            "store",
        ] {
            assert!(!delegated(&root, &path(p)), "{p}");
        }
        assert!(!delegated(&root, &[]));
    }

    /// The configuration after `disableNet`, for a system file `system`,
    /// a user file `user` and the command line `argv` (which Nix runs as
    /// `nix --offline …`: `config show` prints 4294967295, 0, 1 and
    /// false for `tarball-ttl`, `filetransfer-retry-attempts`,
    /// `connect-timeout` and `substitute`).
    fn offline(system: &str, user: &str, argv: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let system_file = dir.path().join("nix.conf");
        std::fs::write(&system_file, system).unwrap();
        let user_file = dir.path().join("user.conf");
        std::fs::write(&user_file, user).unwrap();
        let mut conf = brix::nixconf::NixConf::load_from(&brix::nixconf::Sources {
            system_file: Some(system_file),
            user_files: vec![user_file],
            env_text: None,
            nix_remote: None,
        })
        .unwrap();
        let root = tree::build();
        let argv: Vec<String> = argv.split_whitespace().map(str::to_owned).collect();
        let inv = args::parse(&root, &tree::aliases(), &argv).unwrap();
        Settings::from_invocation(&root, &inv).apply_to(&mut conf);
        let mut config = Config {
            daemon_settings: store_config(&conf).unwrap().settings,
            ..Config::default()
        };
        disable_net(&conf, &mut config);
        config
    }

    #[test]
    fn offline_changes_the_network_settings_nobody_set() {
        let config = offline("", "", "--offline eval --expr 1");
        assert_eq!(config.fetch.tarball_ttl, 4_294_967_295);
        assert_eq!(config.fetch.download_attempts, 0);
        assert_eq!(config.fetch.connect_timeout, 1);
        assert_eq!(
            config.daemon_settings.get("substitute").map(String::as_str),
            Some("false")
        );
        // The system file does not count as setting them.
        let system = "tarball-ttl = 5\nfiletransfer-retry-attempts = 2\nconnect-timeout = 3\nsubstitute = true\n";
        let config = offline(system, "", "--offline eval --expr 1");
        assert_eq!(config.fetch.tarball_ttl, 4_294_967_295);
        assert_eq!(config.fetch.download_attempts, 0);
        assert_eq!(config.fetch.connect_timeout, 1);
        assert_eq!(
            config.daemon_settings.get("substitute").map(String::as_str),
            Some("false")
        );
    }

    /// What the user set, in a user file or on the command line (under
    /// any alias), stays.
    #[test]
    fn offline_keeps_the_network_settings_the_user_set() {
        let user = "tarball-ttl = 5\ndownload-attempts = 2\n";
        let config = offline(
            "",
            user,
            "--offline --connect-timeout 3 --substitute eval --expr 1",
        );
        // `disable_net` leaves these as they were (`Ctx::config` reads
        // their values from the configuration).
        let defaults = Config::default().fetch;
        assert_eq!(config.fetch.tarball_ttl, defaults.tarball_ttl);
        assert_eq!(config.fetch.download_attempts, defaults.download_attempts);
        assert_eq!(config.fetch.connect_timeout, defaults.connect_timeout);
        assert_eq!(
            config.daemon_settings.get("substitute").map(String::as_str),
            Some("true")
        );
    }
}
