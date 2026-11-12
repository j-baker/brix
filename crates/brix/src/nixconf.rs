//! The settings brix reads from Nix's configuration files.
//!
//! Nix loads, in this order, each later source overriding earlier ones
//! (`loadConfFile` in `src/libstore/globals.cc`): the system file
//! `$NIX_CONF_DIR/nix.conf` (`/etc/nix/nix.conf`), the user files from
//! `$NIX_USER_CONF_FILES` (colon-separated) or, when that is unset,
//! `nix.conf` in [`nix_config_dir`] and in `<dir>/nix` for each
//! `$XDG_CONFIG_DIRS` entry (applied last-to-first so the home file wins),
//! then the `NIX_CONFIG` environment variable, and finally the command
//! line. Below them all, `NIX_REMOTE` is the default of the `store`
//! setting (`Settings::storeUri`), which is `auto` without it. A file
//! that cannot be read is skipped; a malformed line, and a value the
//! setting's type rejects (for the settings brix knows the type of,
//! [`check_setting`]), is a usage error. Lines are `name = value…`
//! (the value tokens are joined by single spaces), `#` starts a comment,
//! `include FILE` splices another file and `!include FILE` does so only
//! if it exists; `extra-NAME` appends to `NAME` when `NAME` is a list
//! setting and is an unknown setting otherwise (`Config::set`).
//!
//! Settings from the system file are not *overridden*: a client sends
//! the daemon only overridden settings (`RemoteStore::setOptions`), since
//! the daemon has read that file itself. [`NixConf::is_overridden`] tells
//! them apart.
//!
//! brix honours the settings that affect fetching (`netrc-file`,
//! `access-tokens`, `tarball-ttl`, `http-connections`, `connect-timeout`,
//! `allow-dirty`, `warn-dirty`, `flake-registry`) and those that choose and configure
//! the store (`store` and the settings a client forwards to its daemon,
//! such as `max-jobs` and `substituters`), so that brix and the `nix` it
//! hands commands to use the same store the same way. Evaluation settings
//! keep their command-line-only behaviour, so the test oracle's
//! configuration cannot leak into comparisons.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use brix_store::setting;

use crate::error::{Error, Result};

/// Where configuration comes from, so tests can supply their own.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// `$NIX_CONF_DIR/nix.conf`.
    pub system_file: Option<PathBuf>,
    /// User files, highest priority first (the order `nixUserConfFiles`
    /// returns; they are applied in reverse).
    pub user_files: Vec<PathBuf>,
    /// The `NIX_CONFIG` text.
    pub env_text: Option<String>,
    /// `NIX_REMOTE`: the default of the `store` setting, in place of
    /// `auto`, which every configuration source overrides.
    pub nix_remote: Option<OsString>,
}

/// The environment Nix's directories are resolved in: the process's, or
/// a map in tests.
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// `nixConfDir()`: `$NIX_CONF_DIR` if set and not empty, else Nix's
/// compiled-in `/etc/nix`. Holds the system `nix.conf` and the system
/// flake registry.
pub fn nix_conf_dir() -> PathBuf {
    nix_conf_dir_in(&process_env)
}

fn nix_conf_dir_in(env: Env<'_>) -> PathBuf {
    match env("NIX_CONF_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/etc/nix"),
    }
}

/// `getConfigDir()`: `$NIX_CONFIG_HOME`, else `$XDG_CONFIG_HOME/nix`,
/// else `$HOME/.config/nix` (each variable only if it is an absolute
/// path, as `getEnvOsAbsPath` has it). Holds the user's `nix.conf` and
/// flake registry.
pub fn nix_config_dir() -> PathBuf {
    nix_config_dir_in(&process_env)
}

fn nix_config_dir_in(env: Env<'_>) -> PathBuf {
    let abs = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(dir) = abs("NIX_CONFIG_HOME") {
        return dir;
    }
    let config_home = abs("XDG_CONFIG_HOME").unwrap_or_else(|| {
        abs("HOME")
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".config")
    });
    config_home.join("nix")
}

impl Sources {
    /// The sources Nix would use in this process's environment.
    pub fn from_environment() -> Sources {
        Sources::from_env(&process_env)
    }

    fn from_env(env: Env<'_>) -> Sources {
        let user_files = match env("NIX_USER_CONF_FILES") {
            Some(list) => std::env::split_paths(&list).collect(),
            None => {
                // `getConfigDirs()`: the config directory, then `<dir>/nix`
                // for each absolute `$XDG_CONFIG_DIRS` entry.
                let mut dirs = vec![nix_config_dir_in(env)];
                let config_dirs = env("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
                for d in std::env::split_paths(&config_dirs) {
                    if d.is_absolute() {
                        dirs.push(d.join("nix"));
                    }
                }
                dirs.into_iter().map(|d| d.join("nix.conf")).collect()
            }
        };
        Sources {
            system_file: Some(nix_conf_dir_in(env).join("nix.conf")),
            user_files,
            // Nix reads the text as bytes; the settings brix reads are
            // ASCII, so stray non-UTF-8 bytes elsewhere do not matter.
            env_text: env("NIX_CONFIG").map(|t| t.to_string_lossy().into_owned()),
            nix_remote: env("NIX_REMOTE"),
        }
    }
}

/// The list settings brix reads, with Nix's defaults: `extra-NAME`
/// appends to these (starting from the default when nothing set them)
/// and is an unknown setting for any other `NAME` (`isAppendable`).
const APPENDABLE: [(&str, &str); 3] = [
    ("access-tokens", ""),
    ("substituters", "https://cache.nixos.org/"),
    (
        "trusted-public-keys",
        "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
    ),
];

/// The other names of Nix's global settings (`AbstractSetting::aliases`),
/// with the setting each one names. `Config::set` gives a value set under
/// an alias, in `nix.conf` or with `--option`, to that setting, so an
/// alias is never an unknown setting, and `extra-ALIAS` appends to a list
/// setting as `extra-NAME` does.
pub const SETTING_ALIASES: &[(&str, &str)] = &[
    ("binary-cache-public-keys", "trusted-public-keys"),
    ("binary-caches", "substituters"),
    ("binary-caches-parallel-connections", "http-connections"),
    ("build-chroot-dirs", "sandbox-paths"),
    ("build-compress-log", "compress-build-log"),
    ("build-cores", "cores"),
    ("build-fallback", "fallback"),
    ("build-impersonate-linux-26", "impersonate-linux-26"),
    ("build-keep-log", "keep-build-log"),
    ("build-max-jobs", "max-jobs"),
    ("build-max-log-size", "max-build-log-size"),
    ("build-max-silent-time", "max-silent-time"),
    ("build-sandbox-paths", "sandbox-paths"),
    ("build-timeout", "timeout"),
    ("build-use-chroot", "sandbox"),
    ("build-use-sandbox", "sandbox"),
    ("build-use-substitutes", "substitute"),
    ("commit-lockfile-summary", "commit-lock-file-summary"),
    ("download-attempts", "filetransfer-retry-attempts"),
    ("gc-keep-derivations", "keep-derivations"),
    ("gc-keep-outputs", "keep-outputs"),
    ("substitution-max-jobs", "max-substitution-jobs"),
    ("trusted-binary-caches", "trusted-substituters"),
];

/// The name the setting `name` is kept under: the setting an alias names
/// ([`SETTING_ALIASES`]), `extra-` kept, and any other name as it is.
pub fn canonical_setting_name(name: &str) -> std::borrow::Cow<'_, str> {
    let (extra, base) = match name.strip_prefix("extra-") {
        Some(base) => ("extra-", base),
        None => ("", name),
    };
    match SETTING_ALIASES.iter().find(|(alias, _)| *alias == base) {
        Some((_, setting)) => format!("{extra}{setting}").into(),
        None => name.into(),
    }
}

/// `tokenizeString` with its default separators: Nix splits setting
/// values and configuration lines on these four only.
fn tokenize(s: &str) -> impl Iterator<Item = &str> {
    s.split([' ', '\t', '\n', '\r']).filter(|t| !t.is_empty())
}

/// One setting's value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Value {
    text: String,
    /// Set by something other than the system file (`overridden`).
    overridden: bool,
}

/// The merged settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NixConf {
    values: BTreeMap<String, Value>,
}

impl NixConf {
    /// Loads the configuration the way Nix does in this environment.
    pub fn load() -> Result<NixConf> {
        NixConf::load_from(&Sources::from_environment())
    }

    /// Loads from explicit sources (`loadConfFile`).
    pub fn load_from(sources: &Sources) -> Result<NixConf> {
        let mut conf = NixConf::default();
        // `Settings::storeUri`'s default, which is not an override.
        if let Some(remote) = &sources.nix_remote {
            let remote = remote.to_str().ok_or_else(|| {
                Error::Config(format!(
                    "NIX_REMOTE={remote:?} is not valid UTF-8, which brix does not support in a store URI"
                ))
            })?;
            conf.set("store", remote)?;
        }
        if let Some(f) = &sources.system_file {
            conf.apply_file(f)?;
        }
        // Only what comes after the system file is sent to a daemon.
        conf.reset_overridden();
        for f in sources.user_files.iter().rev() {
            conf.apply_file(f)?;
        }
        if let Some(text) = &sources.env_text {
            conf.apply_text(text, Path::new("NIX_CONFIG"))?;
        }
        Ok(conf)
    }

    /// Applies one file; an unreadable file is skipped like Nix does.
    fn apply_file(&mut self, path: &Path) -> Result<()> {
        match std::fs::read_to_string(path) {
            Ok(text) => self.apply_text(&text, path),
            Err(_) => Ok(()),
        }
    }

    /// `applyConfig`: `parseConfigFiles` followed by `set` for every pair.
    pub fn apply_text(&mut self, text: &str, path: &Path) -> Result<()> {
        for (name, value) in parse(text, path)? {
            self.set(&name, &value)?;
        }
        Ok(())
    }

    /// `Config::set`: sets one setting and marks it overridden. An alias
    /// sets the setting it names ([`canonical_setting_name`]).
    /// `extra-NAME` appends to a list setting `NAME` (a space-separated
    /// value); for any other `NAME` it is a setting of its own name,
    /// which nothing reads. A value the setting's type rejects is Nix's
    /// `UsageError` ([`check_setting`]), and changes nothing.
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        let name = &*canonical_setting_name(name);
        check_setting(name, value)?;
        let appendable = name
            .strip_prefix("extra-")
            .and_then(|base| APPENDABLE.iter().find(|(n, _)| *n == base));
        match appendable {
            Some((base, default)) => {
                let entry = self
                    .values
                    .entry((*base).to_owned())
                    .or_insert_with(|| Value {
                        text: (*default).to_owned(),
                        overridden: true,
                    });
                if !entry.text.is_empty() && !value.is_empty() {
                    entry.text.push(' ');
                }
                entry.text.push_str(value);
                entry.overridden = true;
            }
            None => {
                self.values.insert(
                    name.to_owned(),
                    Value {
                        text: value.to_owned(),
                        overridden: true,
                    },
                );
            }
        }
        Ok(())
    }

    /// `Config::resetOverridden`.
    fn reset_overridden(&mut self) {
        for v in self.values.values_mut() {
            v.overridden = false;
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(|v| v.text.as_str())
    }

    /// Whether `name` was set by a user file, `NIX_CONFIG` or the command
    /// line, rather than only by the system file or not at all: what a
    /// client sends its daemon (`getSettings(…, overriddenOnly = true)`).
    pub fn is_overridden(&self, name: &str) -> bool {
        self.values.get(name).is_some_and(|v| v.overridden)
    }

    /// The overridden settings (see [`NixConf::is_overridden`]), by name.
    pub fn overridden(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .filter(|(_, v)| v.overridden)
            .map(|(k, v)| (k.as_str(), v.text.as_str()))
    }

    /// An integer setting of type `size_t` or `uint64_t`
    /// (`BaseSetting<N>::parse`).
    pub fn get_u64(&self, name: &str) -> Result<Option<u64>> {
        self.get(name).map(|v| parse_int(name, v)).transpose()
    }

    /// An integer setting of type `unsigned int` (`tarball-ttl`).
    pub fn get_u32(&self, name: &str) -> Result<Option<u32>> {
        self.get(name).map(|v| parse_int(name, v)).transpose()
    }

    /// A boolean setting, parsed like `BaseSetting<bool>::parse`.
    pub fn get_bool(&self, name: &str) -> Result<Option<bool>> {
        match self.get(name) {
            None => Ok(None),
            Some(value) => parse_bool(name, value).map(Some),
        }
    }

    /// `netrc-file`, defaulting to `$NIX_CONF_DIR/netrc` as Nix does.
    pub fn netrc_file(&self) -> PathBuf {
        match self.get("netrc-file") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => nix_conf_dir().join("netrc"),
        }
    }

    /// `access-tokens`: `host=token` pairs (`BaseSetting<StringMap>`).
    /// The first pair for a host wins, within one value as across
    /// `extra-access-tokens` additions; a token without `=` is ignored.
    pub fn access_tokens(&self) -> BTreeMap<String, String> {
        let mut tokens = BTreeMap::new();
        for pair in tokenize(self.get("access-tokens").unwrap_or("")) {
            if let Some((host, token)) = pair.split_once('=') {
                tokens
                    .entry(host.to_owned())
                    .or_insert_with(|| token.to_owned());
            }
        }
        tokens
    }
}

/// A boolean setting's value, parsed like `BaseSetting<bool>::parse`.
pub fn parse_bool(name: &str, value: &str) -> Result<bool> {
    Ok(setting::parse_bool(name, value)?)
}

/// An integer setting of type `N` (`unsigned int`, `size_t`, `time_t`,
/// …), parsed like `BaseSetting<N>::parse`: `string2IntWithUnitPrefix`,
/// decimal with an optional `K`, `M`, `G` or `T` suffix.
pub fn parse_int<N: setting::SettingInt>(name: &str, value: &str) -> Result<N> {
    Ok(setting::parse_int(name, value)?)
}

/// `MaxBuildJobsSetting::parse`: `auto` (one per CPU) or an `unsigned
/// int`, without unit suffixes.
pub fn parse_max_jobs(value: &str) -> Result<u32> {
    if value == "auto" {
        return Ok(std::thread::available_parallelism().map_or(1, |n| n.get() as u32));
    }
    value.parse::<u32>().map_err(|_| {
        Error::Usage("configuration setting 'max-jobs' should be 'auto' or an integer".into())
    })
}

/// The type a setting brix reads or forwards has in Nix, which decides
/// how its value is parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingType {
    /// `bool`.
    Bool,
    /// `unsigned int`, `uint32_t`.
    U32,
    /// `size_t`, `uint64_t`.
    U64,
    /// `time_t`.
    I64,
    /// `MaxBuildJobsSetting`.
    MaxJobs,
    /// `SandboxMode`: `true`, `false` or `relaxed`.
    Sandbox,
    /// `Diagnose`: the `lint-*` settings.
    Diagnose,
}

/// Every setting brix reads (from the command line or `nix.conf`) or
/// forwards to its daemon whose value is not free text, with its type.
const SETTING_TYPES: &[(&str, SettingType)] = &[
    ("allow-dirty", SettingType::Bool),
    ("allow-dirty-locks", SettingType::Bool),
    ("allow-import-from-derivation", SettingType::Bool),
    ("connect-timeout", SettingType::U64),
    ("cores", SettingType::U32),
    ("eval-cores", SettingType::U32),
    ("fallback", SettingType::Bool),
    ("filetransfer-retry-attempts", SettingType::U32),
    ("filetransfer-retry-delay", SettingType::U32),
    ("http-connections", SettingType::U64),
    ("impersonate-linux-26", SettingType::Bool),
    ("keep-failed", SettingType::Bool),
    ("keep-going", SettingType::Bool),
    ("lint-absolute-path-literals", SettingType::Diagnose),
    ("lint-short-path-literals", SettingType::Diagnose),
    ("lint-url-literals", SettingType::Diagnose),
    ("max-call-depth", SettingType::U32),
    ("max-jobs", SettingType::MaxJobs),
    ("max-silent-time", SettingType::I64),
    ("nix-219-compat", SettingType::Bool),
    ("print-missing", SettingType::Bool),
    ("pure-eval", SettingType::Bool),
    ("require-sigs", SettingType::Bool),
    ("sandbox", SettingType::Sandbox),
    ("show-trace", SettingType::Bool),
    ("substitute", SettingType::Bool),
    ("tarball-ttl", SettingType::U32),
    ("timeout", SettingType::I64),
    ("trust-tarballs-from-git-forges", SettingType::Bool),
    ("use-registries", SettingType::Bool),
    ("warn-dirty", SettingType::Bool),
    ("warn-short-path-literals", SettingType::Bool),
];

/// The type of the setting `name`, if brix knows it.
pub fn setting_type(name: &str) -> Option<SettingType> {
    SETTING_TYPES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, t)| t)
}

/// `AbstractSetting::set`'s parse, for the settings brix knows the type
/// of: `value` is valid for `name`, or Nix's `UsageError` for it. A
/// setting of another name takes any value here.
pub fn check_setting(name: &str, value: &str) -> Result<()> {
    match setting_type(name) {
        None => Ok(()),
        Some(SettingType::Bool) => parse_bool(name, value).map(drop),
        Some(SettingType::U32) => parse_int::<u32>(name, value).map(drop),
        Some(SettingType::U64) => parse_int::<u64>(name, value).map(drop),
        Some(SettingType::I64) => parse_int::<i64>(name, value).map(drop),
        Some(SettingType::MaxJobs) => parse_max_jobs(value).map(drop),
        Some(SettingType::Sandbox) => match value {
            "true" | "false" | "relaxed" => Ok(()),
            _ => Err(Error::Usage(
                format!("option '{name}' has invalid value '{value}'").into(),
            )),
        },
        Some(SettingType::Diagnose) => parse_diagnose(name, value).map(drop),
    }
}

/// `BaseSetting<Diagnose>::parse`.
pub fn parse_diagnose(name: &str, value: &str) -> Result<brix_syntax::Diagnose> {
    brix_syntax::Diagnose::parse(value).ok_or_else(|| {
        Error::Usage(
            format!(
            "option '{name}' has invalid value '{value}' (expected 'ignore', 'warn', or 'fatal')"
        )
            .into(),
        )
    })
}

/// `parseConfigFiles`: the `(name, value)` pairs of `text` and its includes.
fn parse(text: &str, path: &Path) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for raw in text.split('\n') {
        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let tokens: Vec<&str> = tokenize(line).collect();
        if tokens.is_empty() {
            continue;
        }
        let syntax_error = || {
            Error::Usage(
                format!(
                    "syntax error in configuration line '{}' in \"{}\"",
                    line,
                    path.display()
                )
                .into(),
            )
        };
        if tokens.len() < 2 {
            return Err(syntax_error());
        }
        let include = match tokens[0] {
            "include" => Some(false),
            "!include" => Some(true),
            _ => None,
        };
        if let Some(ignore_missing) = include {
            if tokens.len() != 2 {
                return Err(syntax_error());
            }
            let parent = path.parent().unwrap_or(Path::new("."));
            let included = parent.join(tokens[1]);
            if included.exists() {
                if let Ok(t) = std::fs::read_to_string(&included) {
                    pairs.extend(parse(&t, &included)?);
                }
            } else if !ignore_missing {
                return Err(Error::Config(format!(
                    "file \"{}\" included from \"{}\" not found",
                    included.display(),
                    path.display()
                )));
            }
            continue;
        }
        if tokens[1] != "=" {
            return Err(syntax_error());
        }
        pairs.push((tokens[0].to_owned(), tokens[2..].join(" ")));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn from_text(text: &str) -> NixConf {
        let mut conf = NixConf::default();
        conf.apply_text(text, Path::new("nix.conf")).unwrap();
        conf
    }

    #[test]
    fn precedence_system_then_user_then_env() {
        let dir = tempfile::tempdir().unwrap();
        let system = write(
            dir.path(),
            "system.conf",
            "netrc-file = /sys/netrc # system\naccess-tokens = github.com=sys\n",
        );
        let user_low = write(dir.path(), "xdg.conf", "netrc-file = /xdg/netrc\n");
        let user_high = write(dir.path(), "home.conf", "netrc-file = /home/netrc\n");
        let conf = NixConf::load_from(&Sources {
            system_file: Some(system),
            user_files: vec![user_high, user_low],
            env_text: Some("extra-access-tokens = gitlab.com=env".into()),
            nix_remote: None,
        })
        .unwrap();
        assert_eq!(conf.netrc_file(), PathBuf::from("/home/netrc"));
        assert_eq!(
            conf.access_tokens(),
            BTreeMap::from([
                ("github.com".to_owned(), "sys".to_owned()),
                ("gitlab.com".to_owned(), "env".to_owned()),
            ])
        );
    }

    /// `NIX_REMOTE` is the `store` setting's default: every file and
    /// `NIX_CONFIG` override it, and it is not sent to a daemon.
    #[test]
    fn nix_remote_is_the_default_store() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(dir.path(), "user.conf", "store = ssh-ng://user\n");
        let load = |user_files: Vec<PathBuf>, env_text: Option<&str>| {
            NixConf::load_from(&Sources {
                user_files,
                env_text: env_text.map(str::to_owned),
                nix_remote: Some("unix:///remote".into()),
                ..Sources::default()
            })
            .unwrap()
        };
        let conf = load(Vec::new(), None);
        assert_eq!(conf.get("store"), Some("unix:///remote"));
        assert!(!conf.is_overridden("store"));
        assert_eq!(
            load(vec![user.clone()], None).get("store"),
            Some("ssh-ng://user")
        );
        assert_eq!(
            load(vec![user], Some("store = daemon")).get("store"),
            Some("daemon")
        );
        let err = NixConf::load_from(&Sources {
            nix_remote: Some(std::os::unix::ffi::OsStringExt::from_vec(b"\xff".to_vec())),
            ..Sources::default()
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "NIX_REMOTE=\"\\xFF\" is not valid UTF-8, which brix does not support in a store URI"
        );
    }

    /// `loadConfFile` calls `resetOverridden()` after the system file:
    /// only later sources (and the command line) override.
    #[test]
    fn only_settings_after_the_system_file_are_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let system = write(
            dir.path(),
            "system.conf",
            "max-jobs = 8\nsubstituters = https://sys\nsandbox = true\n",
        );
        let user = write(dir.path(), "user.conf", "sandbox = false\n");
        let mut conf = NixConf::load_from(&Sources {
            system_file: Some(system),
            user_files: vec![user],
            env_text: Some("extra-substituters = https://env".into()),
            nix_remote: None,
        })
        .unwrap();
        conf.set("timeout", "5").unwrap();
        assert_eq!(conf.get("max-jobs"), Some("8"));
        assert!(!conf.is_overridden("max-jobs"));
        assert!(!conf.is_overridden("unset"));
        assert_eq!(conf.get("substituters"), Some("https://sys https://env"));
        assert_eq!(
            conf.overridden().collect::<Vec<_>>(),
            [
                ("sandbox", "false"),
                ("substituters", "https://sys https://env"),
                ("timeout", "5"),
            ]
        );
    }

    /// `BaseSetting<StringMap>`: the first pair for a host wins, and an
    /// addition never replaces a host that is already there; setting the
    /// value again starts over.
    #[test]
    fn access_tokens_first_wins() {
        let mut conf =
            from_text("access-tokens = a=1 a=2 b=3 junk\nextra-access-tokens = a=9 c=4\n");
        let tokens = |c: &NixConf| {
            c.access_tokens()
                .into_iter()
                .map(|(h, t)| format!("{h}={t}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(tokens(&conf), ["a=1", "b=3", "c=4"]);
        conf.set("access-tokens", "a=5").unwrap();
        assert_eq!(tokens(&conf), ["a=5"]);
    }

    /// `extra-NAME` appends only to list settings, starting from Nix's
    /// default; for anything else it is an unknown setting.
    #[test]
    fn extra_appends_to_list_settings_only() {
        let conf = from_text(
            "tarball-ttl = 7\nextra-tarball-ttl = 5\nextra-substituters = https://b\nextra-trusted-public-keys = k:1\nextra-unknown = x\n",
        );
        assert_eq!(conf.get_u64("tarball-ttl").unwrap(), Some(7));
        assert_eq!(
            conf.get("substituters"),
            Some("https://cache.nixos.org/ https://b")
        );
        assert_eq!(
            conf.get("trusted-public-keys"),
            Some("cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= k:1")
        );
        assert_eq!(conf.get("unknown"), None);
        // Set after an addition: replaced.
        let conf = from_text("extra-substituters = https://b\nsubstituters = https://c\n");
        assert_eq!(conf.get("substituters"), Some("https://c"));
    }

    /// An alias sets the setting it names, which is what a daemon is sent
    /// (`getSettings` leaves aliases out); `extra-ALIAS` appends to a list
    /// setting and is unknown otherwise.
    #[test]
    fn aliases_set_the_setting_they_name() {
        let conf = from_text(
            "build-max-jobs = 4\ndownload-attempts = 2\nbinary-caches = https://a\nextra-binary-caches = https://b\nextra-build-cores = 3\n",
        );
        assert_eq!(
            conf.overridden().collect::<Vec<_>>(),
            [
                ("extra-cores", "3"),
                ("filetransfer-retry-attempts", "2"),
                ("max-jobs", "4"),
                ("substituters", "https://a https://b"),
            ]
        );
        let err = NixConf::default()
            .set("download-attempts", "x")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "setting 'filetransfer-retry-attempts' has invalid value 'x'"
        );
    }

    /// `string2IntWithUnitPrefix`, with Nix's error for anything else.
    #[test]
    fn integers_parse_like_nix() {
        let conf = from_text(
            "a = 2K\nb = 1g\nc = +3\nd = 5x\ne = -1\nf = 4G\ng = 4294967296\nh = 12\ni = K\n",
        );
        assert_eq!(conf.get_u64("a").unwrap(), Some(2048));
        assert_eq!(conf.get_u64("b").unwrap(), Some(1 << 30));
        assert_eq!(conf.get_u64("c").unwrap(), Some(3));
        assert_eq!(conf.get_u64("h").unwrap(), Some(12));
        assert_eq!(conf.get_u64("absent").unwrap(), None);
        for name in ["d", "e", "i"] {
            assert_eq!(
                conf.get_u64(name).unwrap_err().to_string(),
                format!(
                    "setting '{name}' has invalid value '{}'",
                    conf.get(name).unwrap()
                )
            );
        }
        // An `unsigned int` setting: the number must fit, the product wraps.
        assert_eq!(conf.get_u32("f").unwrap(), Some(0));
        assert_eq!(conf.get_u64("f").unwrap(), Some(4 << 30));
        assert!(conf.get_u32("g").is_err());
    }

    /// Lines and values split on Nix's four separators only.
    #[test]
    fn values_split_on_nix_separators() {
        let conf =
            from_text("substituters =\ta\u{a0}b \r c\u{b}d\naccess-tokens = h=x\u{a0}y\th2=z\n");
        assert_eq!(conf.get("substituters"), Some("a\u{a0}b c\u{b}d"));
        assert_eq!(
            conf.access_tokens(),
            BTreeMap::from([
                ("h".to_owned(), "x\u{a0}y".to_owned()),
                ("h2".to_owned(), "z".to_owned()),
            ])
        );
    }

    #[test]
    fn includes_and_values_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "extra.conf", "tarball-ttl = 7\n");
        let main = write(
            dir.path(),
            "nix.conf",
            "include extra.conf\n!include missing.conf\nsubstituters =   a   b  # spaced\n",
        );
        let conf = NixConf::load_from(&Sources {
            system_file: Some(main),
            ..Sources::default()
        })
        .unwrap();
        assert_eq!(conf.get_u64("tarball-ttl").unwrap(), Some(7));
        assert_eq!(conf.get("substituters"), Some("a b"));
    }

    /// A value the setting's type rejects is Nix's `UsageError` when it
    /// is set, and leaves the setting as it was; settings brix does not
    /// know take anything.
    #[test]
    fn known_settings_are_checked_when_set() {
        let mut conf = NixConf::default();
        assert_eq!(conf.get_bool("warn-dirty").unwrap(), None);
        for (text, value) in [("yes", true), ("1", true), ("no", false), ("false", false)] {
            conf.set("warn-dirty", text).unwrap();
            assert_eq!(conf.get_bool("warn-dirty").unwrap(), Some(value));
        }
        for (name, value, message) in [
            ("warn-dirty", "maybe", "Boolean setting 'warn-dirty' has invalid value 'maybe'"),
            ("max-call-depth", "abc", "setting 'max-call-depth' has invalid value 'abc'"),
            ("cores", "4294967296", "setting 'cores' has invalid value '4294967296'"),
            ("max-jobs", "2K", "configuration setting 'max-jobs' should be 'auto' or an integer"),
            ("sandbox", "bogus", "option 'sandbox' has invalid value 'bogus'"),
            (
                "lint-url-literals",
                "bogus",
                "option 'lint-url-literals' has invalid value 'bogus' (expected 'ignore', 'warn', or 'fatal')",
            ),
        ] {
            let err = conf.set(name, value).unwrap_err();
            assert_eq!(err.to_string(), message);
            assert!(matches!(err, Error::Usage(_)), "{err:?}");
        }
        assert_eq!(conf.get_bool("warn-dirty").unwrap(), Some(false));
        assert_eq!(conf.get("max-call-depth"), None);
        for (name, value) in [
            ("max-silent-time", "-1"),
            ("http-connections", "1T"),
            ("max-jobs", "auto"),
            ("sandbox", "relaxed"),
            ("lint-url-literals", "fatal"),
            ("anything", "goes"),
        ] {
            conf.set(name, value).unwrap();
            assert_eq!(conf.get(name), Some(value));
        }
        // In a file, the first bad line fails the whole load.
        let err = NixConf::default()
            .apply_text(
                "tarball-ttl = 7\nkeep-going = maybe\n",
                Path::new("nix.conf"),
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Boolean setting 'keep-going' has invalid value 'maybe'"
        );
    }

    #[test]
    fn errors_like_nix() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "nix.conf", "netrc-file\n");
        let err = NixConf::load_from(&Sources {
            system_file: Some(bad.clone()),
            ..Sources::default()
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "syntax error in configuration line 'netrc-file' in \"{}\"",
                bad.display()
            )
        );
        let bad = write(dir.path(), "nix2.conf", "include nowhere.conf\n");
        let err = NixConf::load_from(&Sources {
            system_file: Some(bad),
            ..Sources::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        // Unreadable files are skipped.
        let conf = NixConf::load_from(&Sources {
            system_file: Some(dir.path().join("absent.conf")),
            ..Sources::default()
        })
        .unwrap();
        assert_eq!(conf.get("netrc-file"), None);
    }

    /// `nixConfDir`, `getConfigDir`, `nixUserConfFiles` and `NIX_CONFIG`.
    #[test]
    fn sources_from_the_environment() {
        let sources = |vars: &[(&str, &str)]| {
            let vars: BTreeMap<String, OsString> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
                .collect();
            let env = move |name: &str| vars.get(name).cloned();
            (
                Sources::from_env(&env),
                nix_config_dir_in(&env),
                nix_conf_dir_in(&env),
            )
        };
        let paths = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();

        let (s, config_dir, conf_dir) = sources(&[("HOME", "/home/u")]);
        assert_eq!(conf_dir, PathBuf::from("/etc/nix"));
        assert_eq!(s.system_file, Some(PathBuf::from("/etc/nix/nix.conf")));
        assert_eq!(config_dir, PathBuf::from("/home/u/.config/nix"));
        assert_eq!(
            s.user_files,
            paths(&["/home/u/.config/nix/nix.conf", "/etc/xdg/nix/nix.conf"])
        );
        assert_eq!(s.env_text, None);
        assert_eq!(s.nix_remote, None);

        let (s, config_dir, conf_dir) = sources(&[
            ("HOME", "/home/u"),
            ("NIX_CONF_DIR", "/conf"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("XDG_CONFIG_DIRS", "/a:relative:/b"),
            ("NIX_CONFIG", "max-jobs = 2"),
            ("NIX_REMOTE", "daemon"),
        ]);
        assert_eq!(conf_dir, PathBuf::from("/conf"));
        assert_eq!(s.system_file, Some(PathBuf::from("/conf/nix.conf")));
        assert_eq!(config_dir, PathBuf::from("/xdg/nix"));
        assert_eq!(
            s.user_files,
            paths(&["/xdg/nix/nix.conf", "/a/nix/nix.conf", "/b/nix/nix.conf"])
        );
        assert_eq!(s.env_text.as_deref(), Some("max-jobs = 2"));
        assert_eq!(s.nix_remote, Some("daemon".into()));

        // NIX_CONFIG_HOME wins; relative directories are ignored; an empty
        // XDG_CONFIG_DIRS names none; NIX_USER_CONF_FILES replaces them all.
        let (s, config_dir, conf_dir) = sources(&[
            ("HOME", "/home/u"),
            ("NIX_CONF_DIR", ""),
            ("NIX_CONFIG_HOME", "/nixhome"),
            ("XDG_CONFIG_HOME", "relative"),
            ("XDG_CONFIG_DIRS", ""),
        ]);
        assert_eq!(conf_dir, PathBuf::from("/etc/nix"));
        assert_eq!(config_dir, PathBuf::from("/nixhome"));
        assert_eq!(s.user_files, paths(&["/nixhome/nix.conf"]));
        let (_, config_dir, _) = sources(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "relative")]);
        assert_eq!(config_dir, PathBuf::from("/home/u/.config/nix"));
        let (s, _, _) = sources(&[("NIX_USER_CONF_FILES", "/one.conf:/two.conf")]);
        assert_eq!(s.user_files, paths(&["/one.conf", "/two.conf"]));
    }
}
