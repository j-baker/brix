//! `brix develop` and `brix print-dev-env` (`src/nix/develop.cc`).
//!
//! The build environment of a derivation is what stdenv's setup script
//! makes of its variables. Like Nix, brix gets it by building a variant of
//! the derivation (`<name>-env`) whose builder runs Nix's `get-env.sh`,
//! which sources the setup script and writes the resulting variables and
//! functions to the output as JSON. `print-dev-env` prints that as a bash
//! script (or the JSON); `develop` starts bash with it as the rc file.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use brix::store::BuildMode;
use brix::{DerivationOutput, FlakeRef, Session, StorePath};
use serde_json::{json, Map, Value};

use crate::commands::realise::{
    build_requests, exec_program, fs_path, output_paths_of, requests_for, require_local_store,
    to_store_paths, MixEnvironment,
};
use crate::commands::{print_stdout, Ctx, LVL_DEBUG, LVL_INFO};
use crate::installable::{
    DerivedPath, ExtendedOutputsSpec, Installable, InstallableSource, OutputsSpec, Resolver,
};
use crate::{CliError, Result};

/// Nix's `get-env.sh`, byte for byte: its store path is an input of every
/// `-env` derivation, so it decides their paths. Nix embeds the file as a
/// raw string that its build opens with a line break
/// (`nix-meson-build-support/generate-header`), hence the leading `\n`.
const GET_ENV_SH: &str = concat!("\n", include_str!("../../data/get-env.sh"));

/// Variables the environment never sets in the shell (`Common::ignoreVars`).
const IGNORE_VARS: [&str; 16] = [
    "BASHOPTS",
    "HOME",
    "NIX_BUILD_TOP",
    "NIX_ENFORCE_PURITY",
    "NIX_LOG_FD",
    "NIX_REMOTE",
    "PPID",
    "SHELLOPTS",
    "SSL_CERT_FILE",
    "TEMP",
    "TEMPDIR",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "UID",
];

/// Colon-separated variables the shell prepends to rather than replaces,
/// so that it stays usable (`makeRcScript`'s `savedVars`).
const SAVED_VARS: [&str; 2] = ["PATH", "XDG_DATA_DIRS"];

/// A bash variable of the build environment.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Var {
    String { exported: bool, value: String },
    Array(Vec<String>),
    Associative(BTreeMap<String, String>),
}

/// `BuildEnvironment`: what `get-env.sh` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BuildEnvironment {
    vars: BTreeMap<String, Var>,
    bash_functions: BTreeMap<String, String>,
    /// The contents of `.attrs.json` and `.attrs.sh`, for derivations
    /// with structured attributes.
    structured_attrs: Option<(String, String)>,
}

impl BuildEnvironment {
    /// `BuildEnvironment::fromJSON`. Variables of other types (`get-env.sh`
    /// writes none) are left out, as in Nix.
    fn from_json(json: &Value) -> Result<BuildEnvironment> {
        let malformed =
            |what: &str| CliError::msg(format!("the build environment is malformed: {what}"));
        let string = |v: &Value, what: &str| -> Result<String> {
            v.as_str().map(str::to_owned).ok_or_else(|| malformed(what))
        };
        let mut vars = BTreeMap::new();
        let variables = json
            .get("variables")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed("no 'variables'"))?;
        for (name, info) in variables {
            let ty = string(&info["type"], "a variable without a type")?;
            let value = &info["value"];
            let var = match ty.as_str() {
                "var" | "exported" => Var::String {
                    exported: ty == "exported",
                    value: string(value, "a string variable that is not a string")?,
                },
                "array" => Var::Array(
                    value
                        .as_array()
                        .ok_or_else(|| malformed("an array variable that is not an array"))?
                        .iter()
                        .map(|v| string(v, "an array element that is not a string"))
                        .collect::<Result<_>>()?,
                ),
                "associative" => Var::Associative(
                    value
                        .as_object()
                        .ok_or_else(|| malformed("an associative array that is not an object"))?
                        .iter()
                        .map(|(k, v)| {
                            Ok((
                                k.clone(),
                                string(v, "an associative value that is not a string")?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                ),
                _ => continue,
            };
            vars.insert(name.clone(), var);
        }
        let bash_functions = json
            .get("bashFunctions")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed("no 'bashFunctions'"))?
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    string(v, "a function body that is not a string")?,
                ))
            })
            .collect::<Result<_>>()?;
        let structured_attrs = match json.get("structuredAttrs") {
            Some(s) => Some((
                string(&s[".attrs.json"], "no '.attrs.json'")?,
                string(&s[".attrs.sh"], "no '.attrs.sh'")?,
            )),
            None => None,
        };
        Ok(BuildEnvironment {
            vars,
            bash_functions,
            structured_attrs,
        })
    }

    /// `BuildEnvironment::toJSON` (`print-dev-env --json`).
    fn to_json(&self) -> Value {
        let variables: Map<String, Value> = self
            .vars
            .iter()
            .map(|(name, var)| {
                let info = match var {
                    Var::String { exported, value } => json!({
                        "type": if *exported { "exported" } else { "var" },
                        "value": value,
                    }),
                    Var::Array(values) => json!({ "type": "array", "value": values }),
                    Var::Associative(values) => json!({ "type": "associative", "value": values }),
                };
                (name.clone(), info)
            })
            .collect();
        let mut out = Map::new();
        out.insert("variables".into(), Value::Object(variables));
        out.insert("bashFunctions".into(), json!(self.bash_functions));
        if let Some((attrs_json, attrs_sh)) = &self.structured_attrs {
            out.insert(
                "structuredAttrs".into(),
                json!({ ".attrs.sh": attrs_sh, ".attrs.json": attrs_json }),
            );
        }
        Value::Object(out)
    }

    /// `BuildEnvironment::toBash`.
    fn to_bash(&self, out: &mut String) {
        for (name, var) in &self.vars {
            if IGNORE_VARS.contains(&name.as_str()) {
                continue;
            }
            match var {
                Var::String { exported, value } => {
                    out.push_str(&format!("{name}={}\n", escape_shell_arg_always(value)));
                    if *exported {
                        out.push_str(&format!("export {name}\n"));
                    }
                }
                Var::Array(values) => {
                    out.push_str(&format!("declare -a {name}=("));
                    for v in values {
                        out.push_str(&escape_shell_arg_always(v));
                        out.push(' ');
                    }
                    out.push_str(")\n");
                }
                Var::Associative(values) => {
                    out.push_str(&format!("declare -A {name}=("));
                    for (k, v) in values {
                        out.push_str(&format!(
                            "[{}]={} ",
                            escape_shell_arg_always(k),
                            escape_shell_arg_always(v)
                        ));
                    }
                    out.push_str(")\n");
                }
            }
        }
        for (name, body) in &self.bash_functions {
            out.push_str(&format!("{name} ()\n{{\n{body}}}\n"));
        }
    }

    fn get_string(&self, name: &str) -> Result<&str> {
        match self.vars.get(name) {
            Some(Var::String { value, .. }) => Ok(value),
            _ => Err(error("bash variable is not a string")),
        }
    }

    /// `getSystem`: the derivation's `system`, else this machine's.
    fn system(&self, default: &str) -> Result<String> {
        match self.vars.get("system") {
            Some(_) => Ok(self.get_string("system")?.to_owned()),
            None => Ok(default.to_owned()),
        }
    }
}

/// `getStrings`: the words of a string, an array's elements, or an
/// associative array's keys.
fn get_strings(var: &Var) -> Vec<String> {
    match var {
        Var::String { value, .. } => value
            .split([' ', '\t', '\n', '\r'])
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect(),
        Var::Array(values) => values.clone(),
        Var::Associative(values) => values.keys().cloned().collect(),
    }
}

/// `escapeShellArgAlways`: single-quoted, `'` written as `'\''`.
fn escape_shell_arg_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `rewriteStrings`: each rewrite in key order, every occurrence, the
/// search resuming where the last replacement began. Nix never finishes
/// when the replacement contains what it replaces; brix says so instead.
fn rewrite_strings(mut s: String, rewrites: &BTreeMap<String, String>) -> Result<String> {
    for (from, to) in rewrites {
        if from == to {
            continue;
        }
        if to.contains(from.as_str()) {
            return Err(error(format!(
                "cannot rewrite '{from}' to '{to}', which contains it"
            )));
        }
        let mut j = 0;
        while let Some(pos) = s[j..].find(from.as_str()) {
            let at = j + pos;
            s.replace_range(at..at + from.len(), to);
            j = at;
        }
    }
    Ok(s)
}

/// A plain error, its message bytes: a failed build's message (which
/// ends with the builder's last log lines) is what the daemon sent.
fn error(message: impl Into<bstr::BString>) -> CliError {
    CliError::msg(message.into())
}

/// What `develop` and `print-dev-env` share (`Common`).
struct Common<'c, 's> {
    ctx: &'c Ctx<'c>,
    session: &'s Session,
    resolver: Resolver<'s>,
    installable: Installable,
}

impl<'c, 's> Common<'c, 's> {
    fn new(ctx: &'c Ctx<'c>, session: &'s Session) -> Result<Common<'c, 's>> {
        if ctx.inv.has("profile") {
            return Err(CliError::msg(
                "'--profile' is not supported: brix does not manage profiles",
            ));
        }
        let resolver = ctx.resolver(session, &["nix-develop"])?;
        let installable = ctx.installable(&resolver)?;
        Ok(Common {
            ctx,
            session,
            resolver,
            installable,
        })
    }

    /// `getBuildEnvironment`: the environment and the store path of the
    /// file it came from.
    fn build_environment(&mut self) -> Result<(BuildEnvironment, StorePath)> {
        let store_dir = self.session.config().store_dir().clone();
        let (shell_out, contents) = self.shell_out()?;
        let contents = match contents {
            Some(c) => c,
            None => self
                .session
                .store_read_regular_file(&shell_out)?
                .ok_or_else(|| {
                    error(format!(
                        "'{}' is not a regular file",
                        store_dir.display(&shell_out)
                    ))
                })?,
        };
        let json: Value = serde_json::from_slice(&contents).map_err(|e| {
            error(format!(
                "reading the environment in '{}': {e}",
                store_dir.display(&shell_out)
            ))
        })?;
        Ok((BuildEnvironment::from_json(&json)?, shell_out))
    }

    /// `getShellOutPath`: an `-env` store path as given, or the output of
    /// the `-env` variant of the installable's derivation (with its
    /// contents, already read).
    fn shell_out(&mut self) -> Result<(StorePath, Option<Vec<u8>>)> {
        if let InstallableSource::StorePath(path) = &self.installable.source {
            if path.as_str().ends_with("-env") {
                return Ok((path.clone(), None));
            }
        }
        let drvs = self
            .resolver
            .derivations(std::slice::from_ref(&self.installable))?;
        if drvs.len() != 1 {
            return Err(error(format!(
                "'{}' needs to evaluate to a single derivation, but it evaluated to {} derivations",
                self.installable.what(self.session.config().store_dir()),
                drvs.len()
            )));
        }
        let drv_path = drvs.into_iter().next().expect("one derivation");
        let (path, contents) = derivation_environment(self.session, &drv_path)?;
        Ok((path, Some(contents)))
    }

    /// `makeRcScript`: the environment as a bash script, its outputs
    /// redirected to `outputs_dir` and the `--redirect`ed paths to their
    /// directories.
    fn rc_script(
        &mut self,
        env: &BuildEnvironment,
        tmp_dir: &Path,
        outputs_dir: &Path,
    ) -> Result<String> {
        let mut out = String::from("unset shellHook\n");
        for var in SAVED_VARS {
            out.push_str(&format!("{var}=${{{var}:-}}\n"));
            out.push_str(&format!("nix_saved_{var}=\"${var}\"\n"));
        }
        env.to_bash(&mut out);
        for var in SAVED_VARS {
            out.push_str(&format!(
                "{var}=\"${var}${{nix_saved_{var}:+:$nix_saved_{var}}}\"\n"
            ));
        }
        out.push_str("export NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\n");
        for var in ["TMP", "TMPDIR", "TEMP", "TEMPDIR"] {
            out.push_str(&format!("export {var}=\"$NIX_BUILD_TOP\"\n"));
        }
        out.push_str("eval \"${shellHook:-}\"\n");
        let script = out;

        // The outputs go to `outputs_dir/<name>`.
        let outputs = env
            .vars
            .get("outputs")
            .ok_or_else(|| error("derivation does not have an 'outputs' attribute"))?;
        let mut rewrites: BTreeMap<String, String> = BTreeMap::new();
        let mut rewrite = |from: String, to: String| {
            rewrites.entry(from).or_insert(to);
        };
        // The script is text in brix, so a directory whose name is not
        // UTF-8 (the current directory's `outputs`) is refused rather than
        // altered.
        let output_dir = |name: &str| -> Result<String> {
            outputs_dir.join(name).into_os_string().into_string().map_err(|dir| {
                use std::os::unix::ffi::OsStrExt;
                error(
                    [
                        &b"the directory '"[..],
                        dir.as_bytes(),
                        b"' is not valid UTF-8, which brix's development environments do not support",
                    ]
                    .concat(),
                )
            })
        };
        if env.structured_attrs.is_some() {
            let Var::Associative(map) = outputs else {
                return Err(error("bash variable is not an associative array"));
            };
            for (name, from) in map {
                rewrite(from.clone(), output_dir(name)?);
            }
        } else {
            for name in get_strings(outputs) {
                if !env.vars.contains_key(&name) {
                    return Err(error(format!(
                        "the build environment has no variable for output '{name}'"
                    )));
                }
                rewrite(env.get_string(&name)?.to_owned(), output_dir(&name)?);
            }
        }

        // `--redirect installable outputs-dir`.
        let store_dir = self.session.config().store_dir().clone();
        let redirects: Vec<Vec<String>> = self
            .ctx
            .inv
            .flag_uses("redirect")
            .map(|values| values.to_vec())
            .collect();
        for redirect in redirects {
            let dir = self.ctx.abs_path_text(&redirect[1])?;
            let installable = self
                .resolver
                .parse(std::slice::from_ref(&redirect[0]))?
                .remove(0);
            for path in self.output_paths(&installable)? {
                let from = store_dir.display(&path);
                if script.contains(&from) {
                    if self.ctx.verbosity() >= LVL_INFO {
                        // `printInfo("redirecting '%s' to '%s'", from,
                        // PathFmt(dir))`: the path comes quoted twice.
                        brix::log::stderr(&format!("redirecting '{from}' to '\"{dir}\"'"));
                    }
                    rewrite(from, dir.clone());
                } else {
                    brix::log::stderr(&format!(
                        "\u{1b}[35;1mwarning:\u{1b}[0m '{}' (path '{from}') is not used by this build environment",
                        installable.what(&store_dir)
                    ));
                }
            }
        }

        // Structured attributes are read from files the shell can reach.
        if let Some((attrs_json, attrs_sh)) = &env.structured_attrs {
            for (ext, var, contents) in [
                ("sh", "NIX_ATTRS_SH_FILE", attrs_sh),
                ("json", "NIX_ATTRS_JSON_FILE", attrs_json),
            ] {
                let target = tmp_dir.join(format!(".attrs.{ext}"));
                std::fs::write(&target, contents).map_err(|e| {
                    brix::Error::io(format!("opening file \"{}\"", target.display()), e)
                })?;
                if !env.vars.contains_key(var) {
                    return Err(error(format!("the build environment does not set '{var}'")));
                }
                rewrite(
                    env.get_string(var)?.to_owned(),
                    target.to_string_lossy().into_owned(),
                );
            }
        }

        rewrite_strings(script, &rewrites)
    }

    /// `Installable::toStorePathSet` with `Realise::Nothing`: what building
    /// would do (`printMissing`), then the paths the installable denotes,
    /// outputs as they would be built.
    fn output_paths(&mut self, installable: &Installable) -> Result<BTreeSet<StorePath>> {
        let derived = self.resolver.derived_paths(installable)?;
        Ok(to_store_paths(self.session, &derived)?
            .into_iter()
            .collect())
    }

    /// `InstallableFlake::nixpkgsFlakeRef`: the flake's own `nixpkgs`
    /// input when it has one, else `flake:nixpkgs`.
    fn nixpkgs_flake_ref(&mut self) -> Result<FlakeRef> {
        if let InstallableSource::Flake { flake_ref, .. } = &self.installable.source {
            let flake_ref = flake_ref.clone();
            let locked = self.resolver.flake(&flake_ref)?.locked();
            if let Some(id) = locked
                .lock_file
                .find_input(&["nixpkgs".to_owned()])
                .map_err(brix::Error::Flake)?
            {
                if let Some(node) = &locked.lock_file.node(id).locked {
                    if node.is_flake {
                        return Ok(node.locked_ref.clone());
                    }
                }
            }
        }
        Ok(self
            .session
            .parse_flake_ref("flake:nixpkgs", self.ctx.cwd_text()?)?)
    }

    /// The local directory of the flake the installable came from
    /// (`resolvedRef.input.getSourcePath()`), if it has one.
    fn flake_source_path(&mut self) -> Result<Option<PathBuf>> {
        let InstallableSource::Flake { flake_ref, .. } = &self.installable.source else {
            return Ok(None);
        };
        let flake_ref = flake_ref.clone();
        let resolved = &self.resolver.flake(&flake_ref)?.locked().flake.resolved_ref;
        Ok(brix_flake::native::source_path(&resolved.input).map_err(brix::Error::Flake)?)
    }
}

/// `getDerivationEnvironment`: builds the `-env` variant of the
/// derivation at `drv_path` and returns its output that holds the
/// environment, with the environment.
fn derivation_environment(session: &Session, drv_path: &StorePath) -> Result<(StorePath, Vec<u8>)> {
    let store_dir = session.config().store_dir().clone();
    let mut drv = session.read_derivation(drv_path)?;
    if base_name(&drv.builder) != b"bash" {
        return Err(error(
            "'nix develop' only works on derivations that use 'bash' as their builder",
        ));
    }
    let get_env = session.store_add_text("get-env.sh", GET_ENV_SH.as_bytes(), &BTreeSet::new())?;
    drv.args = vec![store_dir.display(&get_env).into()];
    // Remove the derivation's checks on its outputs.
    match drv
        .env
        .get_mut(brix_store::derivation::STRUCTURED_ATTRS_ENV.as_bytes())
    {
        Some(text) => {
            let mut attrs: Value = serde_json::from_slice(text).map_err(|e| {
                error(format!(
                    "derivation '{}' has malformed structured attrs: {e}",
                    drv.name
                ))
            })?;
            if let Some(map) = attrs.as_object_mut() {
                map.remove("outputChecks");
            }
            *text = brix::derivation_json::json_dump(&attrs).into();
        }
        None => {
            for name in [
                "allowedReferences",
                "allowedRequisites",
                "disallowedReferences",
                "disallowedRequisites",
            ] {
                drv.env.remove(name.as_bytes());
            }
        }
    }
    drv.env.remove(b"name".as_slice());
    drv.name.push_str("-env");
    drv.env.insert("name".into(), drv.name.as_str().into());
    drv.input_srcs.insert(get_env);
    let names: Vec<bstr::BString> = drv.outputs.keys().cloned().collect();
    for name in names {
        if matches!(
            drv.outputs[&name],
            DerivationOutput::InputAddressed(_) | DerivationOutput::CAFixed(_)
        ) {
            drv.outputs.insert(name.clone(), DerivationOutput::Deferred);
            drv.env.insert(name, "".into());
        }
    }
    session.fill_in_output_paths(&mut drv)?;
    session.store_upload_inputs(&drv)?;
    let env_drv = session.store_write_derivation(&drv)?;

    // `Store::buildPaths`: a failure is the build's error.
    let results = session.store_realise(
        &[DerivedPath::all_outputs(env_drv.clone())],
        BuildMode::Normal,
    )?;
    if let Some(failed) = results.iter().find(|r| !r.result.is_success()) {
        return Err(error(failed.result.error_msg.clone()));
    }
    // `get-env.sh` writes to whichever output it likes: take the first
    // that is a non-empty file.
    for (_, path) in output_paths_of(session, &env_drv, &OutputsSpec::All)? {
        if let Some(contents) = session.store_read_regular_file(&path)? {
            if !contents.is_empty() {
                return Ok((path, contents));
            }
        }
    }
    Err(error("get-env.sh failed to produce an environment"))
}

/// `baseNameOf`: the part after the last `/`, ignoring one trailing `/`.
/// Bytes, like the builder it is applied to.
fn base_name(path: &[u8]) -> &[u8] {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

/// A session for `develop`/`print-dev-env`: as for other commands, but
/// without warnings about dirty Git trees (`Common::preRun`).
fn session(ctx: &Ctx<'_>) -> Result<Session> {
    let mut config = ctx.config()?;
    config.fetch.warn_dirty = false;
    Ok(Session::new(config)?)
}

/// A temporary directory for the structured-attribute files, kept: the
/// shell `develop` starts reads them after brix is gone.
fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("{prefix}-"))
        .tempdir()
        .map_err(|e| brix::Error::io("creating a temporary directory", e))?;
    Ok(dir.keep())
}

/// `nix print-dev-env`.
pub fn print_dev_env(ctx: &Ctx<'_>) -> Result<()> {
    let session = session(ctx)?;
    let mut common = Common::new(ctx, &session)?;
    let (env, _) = common.build_environment()?;
    if ctx.inv.has("json") {
        return ctx.print_json(&env.to_json());
    }
    // Nix deletes this directory when it is done printing.
    let tmp = tempfile::Builder::new()
        .prefix("nix-dev-env-")
        .tempdir()
        .map_err(|e| brix::Error::io("creating a temporary directory", e))?;
    let outputs_dir = fs_path(&ctx.cwd).join("outputs");
    let script = common.rc_script(&env, tmp.path(), &outputs_dir)?;
    print_stdout(&format!("{script}\n"))
}

/// `--phase` and the flags that name one (`--build`, `--check`, …); the
/// last one given wins.
fn phase(ctx: &Ctx<'_>) -> Option<String> {
    let mut phase = None;
    for f in &ctx.inv.flags {
        phase = match f.name.as_str() {
            "phase" => Some(f.values[0].clone()),
            "unpack" => Some("unpack".into()),
            "configure" => Some("configure".into()),
            "build" => Some("build".into()),
            "check" => Some("check".into()),
            "install" => Some("install".into()),
            "installcheck" => Some("installCheck".into()),
            _ => continue,
        };
    }
    phase
}

/// `nix develop`.
pub fn develop(ctx: &Ctx<'_>) -> Result<()> {
    let command: Vec<String> = match ctx.inv.flag("command") {
        Some([]) => return Err(CliError::usage("--command requires at least one argument")),
        Some(c) => c.to_vec(),
        None => Vec::new(),
    };
    let phase = phase(ctx);
    let environment = MixEnvironment::from_flags(ctx.inv)?;
    let session = session(ctx)?;
    let mut common = Common::new(ctx, &session)?;
    let (env, gcroot) = common.build_environment()?;
    let store_dir = session.config().store_dir().clone();

    let rc_file = tempfile::Builder::new()
        .prefix("nix-shell.")
        .rand_bytes(6)
        .tempfile()
        .map_err(|e| brix::Error::io("creating a temporary file", e))?;
    let tmp_dir = temp_dir("nix-develop")?;
    let outputs_dir = fs_path(&ctx.cwd).join("outputs");
    let mut script = common.rc_script(&env, &tmp_dir, &outputs_dir)?;
    if ctx.verbosity() >= LVL_DEBUG {
        script.push_str("set -x\n");
    }
    script.push_str(&format!("command rm -f '{}'\n", rc_file.path().display()));

    let conf = ctx.nix_conf()?;
    if let Some(phase) = &phase {
        if !command.is_empty() {
            return Err(CliError::usage(
                "you cannot use both '--command' and '--phase'",
            ));
        }
        // `foundMakefile` is set by `buildPhase`.
        script.push_str("foundMakefile=1\n");
        script.push_str(&format!(
            "if declare -f runPhase >/dev/null; then\n  runPhase {phase}Phase\nelse\n  runHook {phase}Phase\nfi\n"
        ));
    } else if !command.is_empty() {
        let args: Vec<String> = command.iter().map(|a| escape_shell_arg_always(a)).collect();
        script.push_str(&format!("exec {}\n", args.join(" ")));
    } else {
        script = format!(
            "[ -n \"$PS1\" ] && [ -e ~/.bashrc ] && source ~/.bashrc;\nshopt -u expand_aliases\n{script}\nshopt -s expand_aliases\n"
        );
        let prompt = |name: &str| {
            conf.get(name)
                .filter(|v| !v.is_empty())
                .map(escape_shell_arg_always)
        };
        if let Some(p) = prompt("bash-prompt") {
            script.push_str(&format!("[ -n \"$PS1\" ] && PS1={p};\n"));
        }
        if let Some(p) = prompt("bash-prompt-prefix") {
            script.push_str(&format!("[ -n \"$PS1\" ] && PS1={p}\"$PS1\";\n"));
        }
        if let Some(p) = prompt("bash-prompt-suffix") {
            script.push_str(&format!("[ -n \"$PS1\" ] && PS1+={p};\n"));
        }
    }

    let mut environ = environment.environ()?;
    // Keeps the environment from being garbage-collected while the shell
    // runs.
    environ.set("NIX_GCROOT", store_dir.display(&gcroot));

    // Errors finding the interactive bash are ignored (`catch (Error &)`),
    // a failed build of it is not: that report ends with `Exit`.
    let shell = match common.interactive_bash() {
        Ok(path) => Some(path),
        Err(CliError::Exit(status)) => return Err(CliError::Exit(status)),
        Err(e) => {
            let message = match &e {
                CliError::Brix(e) => {
                    let rendered = e.render_bytes(session.config().eval.show_trace);
                    match rendered.strip_prefix(b"error: ") {
                        Some(message) => message.to_vec(),
                        None => rendered,
                    }
                }
                CliError::UsageWithHint(m) => m.to_vec(),
                CliError::Message(m) => m.to_vec(),
                CliError::Exit(_) => unreachable!("handled above"),
            };
            brix::log::stderr_bytes(
                &[&b"\x1b[31;1merror (ignored): \x1b[0m"[..], &message].concat(),
            );
            None
        }
    };
    let shell_path = shell.clone().unwrap_or_else(|| PathBuf::from("bash"));
    let shell_text = shell_path.to_string_lossy().into_owned();
    environ.set("SHELL", shell_path.clone().into_os_string());
    script.push_str(&format!("SHELL=\"{shell_text}\"\n"));
    if let Some(parent) = shell.as_ref().and_then(|s| s.parent()) {
        script.push_str(&format!("PATH=\"{}${{PATH:+:$PATH}}\"\n", parent.display()));
    }
    let (mut file, rc_path) = rc_file
        .keep()
        .map_err(|e| brix::Error::io("keeping the rc file", e.error))?;
    file.write_all(script.as_bytes())
        .map_err(|e| brix::Error::io(format!("writing '{}'", rc_path.display()), e))?;
    drop(file);

    // A phase or a command runs without an interactive shell after it.
    let arg0 = shell_path
        .file_name()
        .unwrap_or(shell_path.as_os_str())
        .to_owned();
    let rc_path = rc_path.into_os_string();
    let args: Vec<&OsStr> = if phase.is_some() || !command.is_empty() {
        vec![&rc_path]
    } else {
        vec![OsStr::new("--rcfile"), &rc_path]
    };

    // Phases assume they run in the flake's directory.
    if phase.is_some() {
        if let Some(dir) = common.flake_source_path()? {
            std::env::set_current_dir(&dir).map_err(|e| {
                brix::Error::io(format!("chdir to \"{}\" failed", dir.display()), e)
            })?;
        }
    }

    // `execProgramInStore`.
    require_local_store(&session)?;
    let system = env.system(&ctx.system())?;
    set_personality(
        &system,
        conf.get_bool("impersonate-linux-26")?.unwrap_or(false),
    )?;
    Err(exec_program(shell_path.as_os_str(), &arg0, &args, &environ))
}

impl Common<'_, '_> {
    /// The interactive bash of the flake's nixpkgs (`nixpkgs#bashInteractive`,
    /// built), which `develop` prefers to whatever `bash` is on `PATH`.
    fn interactive_bash(&mut self) -> Result<PathBuf> {
        let nixpkgs = self.nixpkgs_flake_ref()?;
        let mut lock_flags = self.ctx.lock_flags(self.session)?;
        lock_flags.input_overrides.clear();
        lock_flags.input_updates.clear();
        let mut resolver = self
            .ctx
            .resolver_with(self.session, lock_flags, &["nix-build"])?;
        let installable = Installable {
            text: format!("{nixpkgs}#bashInteractive").into(),
            source: InstallableSource::Flake {
                flake_ref: nixpkgs,
                fragment: "bashInteractive".into(),
            },
            outputs: ExtendedOutputsSpec::Default,
        };
        // `Installable::toStorePathSet` with `Realise::Outputs`: built, with
        // Nix's report if that fails, and the output paths in order.
        let requests = requests_for(&mut resolver, std::slice::from_ref(&installable))?;
        let built = build_requests(self.ctx, self.session, &requests, BuildMode::Normal)?;
        let outputs: BTreeSet<StorePath> = built
            .into_iter()
            .flat_map(|b| b.outputs.into_iter().map(|(_, p)| p))
            .collect();
        let store_dir = self.session.config().store_dir().clone();
        for path in &outputs {
            let bash = PathBuf::from(store_dir.display(path)).join("bin/bash");
            // `pathExists`: an `lstat` that only a missing path fails.
            match std::fs::symlink_metadata(&bash) {
                Ok(_) => return Ok(bash),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(e) => {
                    return Err(brix::Error::io(
                        format!("getting status of '{}'", bash.display()),
                        e,
                    )
                    .into())
                }
            }
        }
        Err(error(
            "package 'nixpkgs#bashInteractive' does not provide a 'bin/bash'",
        ))
    }
}

/// `linux::setPersonality`: a 32-bit personality for 32-bit systems on a
/// 64-bit machine, optionally a Linux 2.6 kernel version, and no address
/// space randomisation.
#[cfg(target_os = "linux")]
fn set_personality(system: &str, impersonate_linux_26: bool) -> Result<()> {
    use nix::sys::personality::{self, Persona};

    // nix models personality flags, but not execution-domain bits such
    // as PER_LINUX32. Retain those bits when setting or updating it.
    const PER_LINUX32: Persona = Persona::from_bits_retain(0x0008);
    const UNAME26: Persona = Persona::from_bits_retain(0x0020000);
    let current = || {
        // SAFETY: querying the personality takes no pointers and changes
        // no state. nix::personality::get truncates the execution-domain
        // bits, so it cannot be used for this read-modify-write operation.
        let bits = unsafe { libc::personality(0xffffffff) };
        (bits != -1).then(|| Persona::from_bits_retain(bits))
    };
    let machine_is_x86_64 = std::env::consts::ARCH == "x86_64";
    if (system == "i686-linux" && machine_is_x86_64)
        || matches!(system, "armv7l-linux" | "armv6l-linux" | "armv5tel-linux")
    {
        personality::set(PER_LINUX32).map_err(|errno| {
            CliError::Brix(brix::Error::io(
                "cannot set 32-bit personality",
                errno.into(),
            ))
        })?;
    }
    if matches!(system, "i686-linux" | "x86_64-linux") && impersonate_linux_26 {
        if let Some(cur) = current() {
            let _ = personality::set(cur | UNAME26);
        }
    }
    if let Some(cur) = current() {
        let _ = personality::set(cur | Persona::ADDR_NO_RANDOMIZE);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_personality(_system: &str, _impersonate_linux_26: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Updating flags must not clear PER_LINUX32 (which nix's get()
    /// omits). Isolate the personality change from the test runner.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn personality_updates_preserve_the_32_bit_domain() {
        const CHILD: &str = "BRIX_TEST_PERSONALITY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            set_personality("i686-linux", true).unwrap();
            let output = std::process::Command::new("uname")
                .arg("-m")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"i686\n");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::develop::tests::personality_updates_preserve_the_32_bit_domain",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    fn env(json: &str) -> BuildEnvironment {
        BuildEnvironment::from_json(&serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn json_round_trip_drops_version_and_unknown_types() {
        let e = env(
            r#"{"version":1,"bashFunctions":{"f":" echo hi\n"},"variables":{
                "a":{"type":"var","value":"x"},"b":{"type":"exported","value":"y"},
                "c":{"type":"array","value":["1","2"]},"d":{"type":"associative","value":{"k":"v"}},
                "e":{"type":"unknown"}}}"#,
        );
        assert_eq!(
            e.to_json(),
            serde_json::json!({
                "bashFunctions": {"f": " echo hi\n"},
                "variables": {
                    "a": {"type": "var", "value": "x"},
                    "b": {"type": "exported", "value": "y"},
                    "c": {"type": "array", "value": ["1", "2"]},
                    "d": {"type": "associative", "value": {"k": "v"}},
                }
            })
        );
    }

    #[test]
    fn to_bash_quotes_and_skips_ignored_variables() {
        let e = env(r#"{"bashFunctions":{"f":"\n    echo hi\n"},"variables":{
                "HOME":{"type":"exported","value":"/homeless"},
                "a":{"type":"var","value":"it's"},"b":{"type":"exported","value":"y"},
                "c":{"type":"array","value":["1","2 3"]},"d":{"type":"associative","value":{"k":"v"}}}}"#);
        let mut out = String::new();
        e.to_bash(&mut out);
        assert_eq!(
            out,
            "a='it'\\''s'\nb='y'\nexport b\ndeclare -a c=('1' '2 3' )\ndeclare -A d=(['k']='v' )\nf ()\n{\n\n    echo hi\n}\n"
        );
    }

    /// `get-env.sh` decides the path of every `-env` derivation, so it must
    /// be the bytes the reference `nix` embeds: a C string, which the
    /// build's header generator opens with a line break.
    #[test]
    fn get_env_sh_is_the_reference_nix_s() {
        let nix = std::fs::canonicalize(brix_testutil::nix_bin()).expect("BRIX_NIX_BIN exists");
        let binary = std::fs::read(&nix).expect("the reference nix binary is readable");
        let embedded = [b"\0", GET_ENV_SH.as_bytes(), b"\0"].concat();
        assert!(
            binary.windows(embedded.len()).any(|w| w == embedded),
            "{} does not embed brix's get-env.sh (with its leading line break) as a string",
            nix.display()
        );
    }

    #[test]
    fn helpers_match_nix() {
        assert_eq!(base_name(b"/nix/store/x-bash/bin/bash"), b"bash");
        assert_eq!(base_name(b"bash"), b"bash");
        assert_eq!(base_name(b"/bin/bash/"), b"bash");
        let rewrites: BTreeMap<String, String> =
            [("/nix/store/aaa-x".to_owned(), "/w/outputs/out".to_owned())].into();
        assert_eq!(
            rewrite_strings(
                "out=/nix/store/aaa-x; x=/nix/store/aaa-x/bin".into(),
                &rewrites
            )
            .unwrap(),
            "out=/w/outputs/out; x=/w/outputs/out/bin"
        );
        // The search resumes at the replacement, so it can match again.
        let shrink: BTreeMap<String, String> = [("ab".to_owned(), "a".to_owned())].into();
        assert_eq!(rewrite_strings("abb".into(), &shrink).unwrap(), "a");
        // Nix loops forever on these; brix refuses.
        let grow: BTreeMap<String, String> = [("/o".to_owned(), "/o/o".to_owned())].into();
        assert!(rewrite_strings("/o".into(), &grow).is_err());
        assert_eq!(
            get_strings(&Var::String {
                exported: false,
                value: " out  dev\n".into()
            }),
            ["out", "dev"]
        );
    }
}
