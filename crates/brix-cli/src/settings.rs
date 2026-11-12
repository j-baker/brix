//! Configuration settings from the command line: `--option name value`,
//! the per-setting flags `nix` generates from `nix.conf` (`--store`,
//! `--max-jobs`, `--no-pure-eval`, `--extra-experimental-features`, …) and
//! the logging flags. Collected in command-line order so later flags win.
//!
//! Values are checked as Nix's `Setting::set` checks them, for the
//! settings brix knows the type of ([`brix::nixconf::check_setting`]): a
//! setting flag with a bad value is a usage error, which ends the parse;
//! `--option` with one warns and leaves the setting as it was.

use std::collections::{BTreeMap, BTreeSet};

use brix::nixconf::{self, NixConf};
use brix_syntax::{Diagnose, Lints};

use crate::args::{Command, Invocation};
use crate::tree::SETTINGS_CATEGORY;

/// The resolved settings a command runs with.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Every setting as `nix.conf` text, after applying the flags. Only
    /// values their setting's type accepts get here.
    pub values: BTreeMap<String, String>,
    /// The assignments the command line makes, in order, as `nix.conf`
    /// lines would make them: `extra-NAME` keeps its prefix, so applied on
    /// top of the configuration files it appends to their value as in Nix.
    pub assignments: Vec<(String, String)>,
    /// Flags `nix` itself handles outside the settings machinery.
    pub verbosity: i32,
    pub print_build_logs: bool,
    /// `--offline` (`--no-net`); `main` sets it too when the machine has
    /// no Internet connection, as Nix's `main` clears `useNet`.
    pub offline: bool,
    pub refresh: bool,
    pub help: bool,
    pub version: bool,
    /// `--option` names that are not known settings (warned about once
    /// the command line is parsed).
    pub unknown_settings: Vec<String>,
    /// What Nix warns about while it parses the command line, in order:
    /// `--option` values the setting rejects, deprecated settings.
    pub warnings: Vec<String>,
    /// The `UsageError` of a setting flag whose value the setting rejects
    /// (`--max-call-depth abc`). The flags after it were not read.
    pub error: Option<String>,
}

impl Settings {
    /// Reads the settings flags out of a parsed command line. `root` is
    /// the command tree, whose root flags say which names are settings.
    pub fn from_invocation(root: &Command, inv: &Invocation) -> Settings {
        let mut setting_names: BTreeSet<&str> = BTreeSet::new();
        for f in &root.flags {
            if f.category == SETTINGS_CATEGORY {
                setting_names.insert(f.long.as_str());
            }
        }
        let mut s = Settings {
            values: BTreeMap::new(),
            assignments: Vec::new(),
            // Nix's default is `notice` on a terminal and `info`
            // otherwise; 0 stands for the default.
            verbosity: 0,
            print_build_logs: false,
            offline: false,
            refresh: false,
            help: false,
            version: false,
            unknown_settings: Vec::new(),
            warnings: Vec::new(),
            error: None,
        };
        for f in &inv.flags {
            let assigned = match f.name.as_str() {
                "option" => {
                    // An alias sets the setting it names (`Config::set`).
                    let name = nixconf::canonical_setting_name(&f.values[0]);
                    if !setting_names.contains(&*name) {
                        s.unknown_settings.push(f.values[0].clone());
                    }
                    // `--option`'s handler turns the setting's
                    // `UsageError` into a warning.
                    if let Err(e) = s.assign(&setting_names, &name, &f.values[1]) {
                        s.warnings.push(e);
                    }
                    Ok(())
                }
                "verbose" => {
                    s.verbosity += 1;
                    Ok(())
                }
                "quiet" => {
                    s.verbosity -= 1;
                    Ok(())
                }
                "debug" => {
                    s.verbosity = 10;
                    Ok(())
                }
                "print-build-logs" => {
                    s.print_build_logs = true;
                    Ok(())
                }
                "offline" => {
                    s.offline = true;
                    Ok(())
                }
                "refresh" => {
                    s.refresh = true;
                    Ok(())
                }
                "help" => {
                    s.help = true;
                    Ok(())
                }
                "version" => {
                    s.version = true;
                    Ok(())
                }
                "impure" => s.assign(&setting_names, "pure-eval", "false"),
                // `BaseSetting<SandboxMode>::convertToArg`.
                "relaxed-sandbox" => s.assign(&setting_names, "sandbox", "relaxed"),
                "show-trace" => s.assign(&setting_names, "show-trace", "true"),
                name if setting_names.contains(name) => match name.strip_prefix("no-") {
                    Some(rest) if setting_names.contains(rest) && f.values.is_empty() => {
                        s.assign(&setting_names, rest, "false")
                    }
                    _ => {
                        let value = f.values.first().map_or("true", String::as_str);
                        s.assign(&setting_names, name, value)
                    }
                },
                _ => Ok(()),
            };
            if let Err(e) = assigned {
                s.error = Some(e);
                break;
            }
        }
        s
    }

    /// `name = value`, as `Config::set` applies it: the value is checked
    /// against the setting's type first, and a bad one is Nix's
    /// `UsageError` and changes nothing. `extra-NAME`, for a list
    /// setting NAME (one with an `--extra-NAME` flag), appends to it; any
    /// other name is set outright.
    fn assign(
        &mut self,
        setting_names: &BTreeSet<&str>,
        name: &str,
        value: &str,
    ) -> Result<(), String> {
        nixconf::check_setting(name, value).map_err(|e| e.to_string())?;
        if name == "warn-short-path-literals" {
            // `DeprecatedWarnSetting`, as it is set.
            let on = nixconf::parse_bool(name, value).map_err(|e| e.to_string())?;
            self.warnings.push(format!(
                "'warn-short-path-literals' is deprecated, use 'lint-short-path-literals = {}' instead",
                if on { "warn" } else { "ignore" }
            ));
        }
        self.assignments.push((name.to_owned(), value.to_owned()));
        let appends_to = name
            .strip_prefix("extra-")
            .filter(|base| setting_names.contains(name) && setting_names.contains(base));
        match appends_to {
            Some(base) => {
                let cur = self.values.entry(base.to_owned()).or_default();
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(value);
            }
            None => {
                self.values.insert(name.to_owned(), value.to_owned());
            }
        }
        Ok(())
    }

    /// Applies the command line on top of the configuration files, as Nix
    /// does after loading them.
    pub fn apply_to(&self, conf: &mut NixConf) {
        for (name, value) in &self.assignments {
            conf.set(name, value)
                .expect("the command line's assignments were checked when they were made");
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// A boolean setting (`BaseSetting<bool>::parse`), `default` when the
    /// command line does not set it.
    pub fn get_bool(&self, name: &str, default: bool) -> brix::Result<bool> {
        match self.get(name) {
            Some(value) => nixconf::parse_bool(name, value),
            None => Ok(default),
        }
    }

    /// An `unsigned int` setting (`BaseSetting<unsigned int>::parse`).
    pub fn get_u32(&self, name: &str) -> brix::Result<Option<u32>> {
        self.get(name)
            .map(|value| nixconf::parse_int(name, value))
            .transpose()
    }

    /// Space-separated list settings (`experimental-features`), with
    /// `extra-` additions already merged.
    pub fn get_list(&self, name: &str) -> Vec<String> {
        self.get(name)
            .map(|v| v.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    pub fn pure_eval(&self) -> brix::Result<bool> {
        self.get_bool("pure-eval", true)
    }

    pub fn show_trace(&self) -> brix::Result<bool> {
        self.get_bool("show-trace", false)
    }

    /// `system` (the value `--system` sets; `eval-system` wins for
    /// evaluation, as in Nix).
    pub fn system(&self) -> Option<String> {
        self.get("eval-system")
            .filter(|s| !s.is_empty())
            .or_else(|| self.get("system"))
            .map(str::to_owned)
    }

    pub fn experimental_features(&self) -> BTreeSet<String> {
        self.get_list("experimental-features").into_iter().collect()
    }

    /// The `lint-*` settings. The deprecated `warn-short-path-literals`
    /// sets `lint-short-path-literals` unless that is set itself (Nix's
    /// `DeprecatedWarnSetting`).
    pub fn lints(&self) -> brix::Result<Lints> {
        let level = |name: &str| {
            self.get(name)
                .map(|v| nixconf::parse_diagnose(name, v))
                .transpose()
        };
        let mut lints = Lints::default();
        if let Some(l) = level("lint-url-literals")? {
            lints.url_literals = l;
        }
        if let Some(l) = level("lint-absolute-path-literals")? {
            lints.absolute_path_literals = l;
        }
        match level("lint-short-path-literals")? {
            Some(l) => lints.short_path_literals = l,
            None => {
                if let Some(value) = self.get("warn-short-path-literals") {
                    lints.short_path_literals =
                        if nixconf::parse_bool("warn-short-path-literals", value)? {
                            Diagnose::Warn
                        } else {
                            Diagnose::Ignore
                        };
                }
            }
        }
        Ok(lints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse;
    use crate::tree;

    fn settings(argv: &str) -> Settings {
        let root = tree::build();
        let argv: Vec<String> = argv.split_whitespace().map(str::to_owned).collect();
        let inv = parse(&root, &tree::aliases(), &argv).unwrap();
        Settings::from_invocation(&root, &inv)
    }

    #[test]
    fn setting_flags_and_option() {
        let s = settings("--store ssh-ng://x --max-jobs 4 --no-pure-eval --extra-experimental-features ca-derivations --option warn-dirty false eval --expr 1");
        assert_eq!(s.get("store"), Some("ssh-ng://x"));
        assert_eq!(s.get("max-jobs"), Some("4"));
        assert!(!s.pure_eval().unwrap());
        assert!(s.experimental_features().contains("ca-derivations"));
        assert_eq!(s.get("warn-dirty"), Some("false"));
        assert!(s.unknown_settings.is_empty());
    }

    #[test]
    fn assignments_keep_order_and_extra_prefix() {
        let s = settings("--store ssh-ng://x --extra-substituters a --option extra-substituters b --no-substitute eval --expr 1");
        let pairs: Vec<(&str, &str)> = s
            .assignments
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("store", "ssh-ng://x"),
                ("extra-substituters", "a"),
                ("extra-substituters", "b"),
                ("substitute", "false"),
            ]
        );
        // `--option extra-NAME` appends like the `--extra-NAME` flag.
        assert_eq!(s.get("substituters"), Some("a b"));
    }

    /// A setting's aliases set it, with `--option` as in `nix.conf`, and
    /// are flags of their own: the last flag the setting has, as in Nix
    /// (`nix --build-fallback config show fallback` prints `false`, and
    /// `--substituters a --binary-caches b` makes the substituters
    /// `a b`). `extra-ALIAS` is an unknown setting unless the setting is a
    /// list.
    #[test]
    fn setting_aliases() {
        let s = settings(
            "--option download-attempts 3 --option build-max-jobs 2 --option extra-binary-caches a eval --expr 1",
        );
        assert!(s.unknown_settings.is_empty(), "{:?}", s.unknown_settings);
        assert_eq!(s.get("filetransfer-retry-attempts"), Some("3"));
        assert_eq!(s.get("max-jobs"), Some("2"));
        assert_eq!(s.get("substituters"), Some("a"));
        let s = settings("--option extra-download-attempts 3 eval --expr 1");
        assert_eq!(s.unknown_settings, ["extra-download-attempts"]);

        let s = settings("--fallback --build-fallback --substituters a --binary-caches b --download-attempts 7 --build-use-sandbox eval --expr 1");
        assert_eq!(s.get("fallback"), Some("false"));
        assert_eq!(s.get("substituters"), Some("a b"));
        assert_eq!(s.get("filetransfer-retry-attempts"), Some("7"));
        assert_eq!(s.get("sandbox"), Some("relaxed"));
        let s = settings("--option build-fallback true --option binary-caches c eval --expr 1");
        assert_eq!(s.get("fallback"), Some("true"));
        assert_eq!(s.get("substituters"), Some("c"));
        // A bad value is reported under the setting's own name.
        assert_eq!(
            settings("--option download-attempts x eval --expr 1").warnings,
            ["setting 'filetransfer-retry-attempts' has invalid value 'x'"]
        );
        assert_eq!(
            settings("--download-attempts x eval --expr 1")
                .error
                .as_deref(),
            Some("setting 'filetransfer-retry-attempts' has invalid value 'x'")
        );
    }

    #[test]
    fn impure_and_logging() {
        // `--impure` belongs to the command, so it must follow it (as in nix).
        let s = settings("-vvL eval --impure --expr 1");
        assert_eq!(s.verbosity, 2);
        assert!(s.print_build_logs);
        assert!(!s.pure_eval().unwrap());
        assert_eq!(
            settings("--option bogus 1 eval --expr 1").unknown_settings,
            vec!["bogus"]
        );
    }
}
