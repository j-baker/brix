//! Flake registries (`src/libfetchers/registry.cc`).
//!
//! A registry maps indirect references (`flake:nixpkgs`) to direct ones.
//! Nix consults four registries in order: the `--override-flake` flags,
//! the user registry, the system registry and the global one. brix has no
//! configuration directory of its own; the caller supplies whichever
//! registries apply. Pure evaluation of a locked flake never needs one.

use serde_json::Value;

use crate::attrs::{json_to_attrs, Attrs};
use crate::hashes::GitRev;
use crate::input::{FetchSettings, Input};
use crate::{err, Result};

/// `Registry::RegistryType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistryType {
    Flag,
    User,
    System,
    Global,
    Custom,
}

/// `Registry::Entry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub from: Input,
    pub to: Input,
    /// `dir` of the target, kept out of its attrs.
    pub extra_attrs: Attrs,
    /// Only match `from` exactly (no `ref`/`rev` carry-over).
    pub exact: bool,
}

/// `Registry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    pub registry_type: RegistryType,
    pub entries: Vec<Entry>,
}

impl Registry {
    pub fn empty(registry_type: RegistryType) -> Registry {
        Registry {
            registry_type,
            entries: Vec::new(),
        }
    }

    /// `Registry::read(settings, whence, jsonStr, type)`: a version-2
    /// registry. Nix does not fail on a registry it cannot understand: it
    /// warns, through `warn`, and keeps what it read before the problem (an
    /// unsupported version: nothing). An entry whose inputs are invalid is
    /// an error; see [`Registry::read_file`] for what Nix does with it.
    pub fn parse(
        settings: &FetchSettings,
        whence: &str,
        json_str: &str,
        registry_type: RegistryType,
        warn: &dyn Fn(&str),
    ) -> Result<Registry> {
        let mut registry = Registry::empty(registry_type);
        let cannot_parse = |msg: &dyn std::fmt::Display| {
            warn(&format!("cannot parse flake registry '{whence}': {msg}"));
        };
        let json: Value = match serde_json::from_str(json_str) {
            Ok(json) => json,
            Err(e) => {
                cannot_parse(&e);
                return Ok(registry);
            }
        };
        // `json.value("version", 0)`.
        let version = match json.get("version") {
            None => 0,
            Some(v) => match v.as_i64() {
                Some(n) => n,
                None => {
                    cannot_parse(&format!("'version' is {v}, not a number"));
                    return Ok(registry);
                }
            },
        };
        if version != 2 {
            warn(&format!(
                "flake registry '{whence}' has unsupported version {version}"
            ));
            return Ok(registry);
        }
        let flakes = match json.get("flakes") {
            None | Some(Value::Null) => return Ok(registry),
            Some(Value::Array(flakes)) => flakes,
            Some(other) => {
                cannot_parse(&format!("'flakes' is {other}, not a list"));
                return Ok(registry);
            }
        };
        for entry in flakes {
            let Some(entry) = entry.as_object() else {
                cannot_parse(&format!("registry entry {entry} is not an object"));
                return Ok(registry);
            };
            // A missing `from` or `to` is an empty attribute set, which
            // `Input::fromAttrs` refuses.
            let attrs_of = |name: &str| match entry.get(name) {
                Some(v) => json_to_attrs(v),
                None => Ok(Attrs::new()),
            };
            let mut to_attrs = attrs_of("to")?;
            let mut extra_attrs = Attrs::new();
            if let Some(dir) = to_attrs.remove("dir") {
                extra_attrs.insert("dir".into(), dir);
            }
            let exact = match entry.get("exact") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    cannot_parse(&format!("'exact' is {other}, not a Boolean"));
                    return Ok(registry);
                }
            };
            registry.entries.push(Entry {
                from: Input::from_attrs(settings, attrs_of("from")?)?,
                to: Input::from_attrs(settings, to_attrs)?,
                extra_attrs,
                exact,
            });
        }
        Ok(registry)
    }

    /// `Registry::read(settings, path, type)`, given the file's contents:
    /// as [`Registry::parse`], but an invalid entry is only a warning too,
    /// and the registry is then empty.
    pub fn read_file(
        settings: &FetchSettings,
        path: &str,
        contents: &str,
        registry_type: RegistryType,
        warn: &dyn Fn(&str),
    ) -> Registry {
        match Registry::parse(settings, path, contents, registry_type, warn) {
            Ok(registry) => registry,
            Err(e) => {
                warn(&format!(
                    "cannot read flake registry '{path}': {}",
                    e.render()
                ));
                Registry::empty(registry_type)
            }
        }
    }
}

/// `UseRegistries`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UseRegistries {
    /// Every registry.
    All,
    /// Only the flag and global registries (for flake inputs, so a user's
    /// local overrides do not leak into lock files).
    Limited,
    /// None: indirect inputs are an error.
    No,
}

/// `lookupInRegistries`: resolve an input through the registries (in the
/// given order) until it is direct. Returns the resolved input and the
/// extra attrs (`dir`) of the matching entry.
pub fn lookup_in_registries(
    registries: &[Registry],
    input: &Input,
    use_registries: UseRegistries,
) -> Result<(Input, Attrs)> {
    let mut input = input.clone();
    let mut extra_attrs = Attrs::new();
    if use_registries == UseRegistries::No {
        return Ok((input, extra_attrs));
    }
    let mut n = 0;
    'restart: loop {
        n += 1;
        if n > 100 {
            return Err(err!("cycle detected in flake registry for '{}'", input));
        }
        for registry in registries {
            if use_registries == UseRegistries::Limited
                && !matches!(
                    registry.registry_type,
                    RegistryType::Flag | RegistryType::Global
                )
            {
                continue;
            }
            for entry in &registry.entries {
                if entry.exact {
                    if entry.from == input {
                        input = entry.to.clone();
                        extra_attrs = entry.extra_attrs.clone();
                        continue 'restart;
                    }
                } else if entry.from.contains(&input) {
                    let ref_: Option<String> = if entry.from.ref_()?.is_none() {
                        input.ref_()?.map(str::to_string)
                    } else {
                        None
                    };
                    let rev: Option<GitRev> = if entry.from.rev()?.is_none() {
                        input.rev()?
                    } else {
                        None
                    };
                    input = entry.to.apply_overrides(ref_.as_deref(), rev)?;
                    extra_attrs = entry.extra_attrs.clone();
                    continue 'restart;
                }
            }
        }
        break;
    }
    if !input.is_direct() {
        return Err(err!(
            "cannot find flake '{}' in the flake registries",
            input
        ));
    }
    Ok((input, extra_attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"{
  "flakes": [
    {
      "from": { "id": "nixpkgs", "type": "indirect" },
      "to": { "owner": "NixOS", "repo": "nixpkgs", "ref": "nixpkgs-unstable", "type": "github" }
    },
    {
      "from": { "id": "nix", "type": "indirect" },
      "to": { "owner": "NixOS", "repo": "nix", "type": "github", "dir": "sub" },
      "exact": true
    },
    {
      "from": { "id": "alias", "type": "indirect" },
      "to": { "id": "nixpkgs", "type": "indirect" }
    }
  ],
  "version": 2
}"#;

    fn settings() -> FetchSettings {
        FetchSettings::default()
    }

    fn no_warning(msg: &str) {
        panic!("unexpected warning: {msg}");
    }

    fn parse(text: &str, registry_type: RegistryType) -> Registry {
        Registry::parse(&settings(), "r.json", text, registry_type, &no_warning).unwrap()
    }

    #[test]
    fn parse_and_lookup() {
        let reg = parse(REGISTRY, RegistryType::Global);
        assert_eq!(reg.entries.len(), 3);
        let lookup = |s: &str, mode| {
            let input = Input::from_url_str(&settings(), s, true).unwrap();
            lookup_in_registries(std::slice::from_ref(&reg), &input, mode)
        };
        let (resolved, extra) = lookup("flake:nixpkgs", UseRegistries::All).unwrap();
        assert_eq!(
            resolved.to_url_text(false).unwrap(),
            "github:NixOS/nixpkgs/nixpkgs-unstable"
        );
        assert!(extra.is_empty());
        // A ref on the indirect input overrides the registry's ref.
        let (resolved, _) = lookup("flake:nixpkgs/nixos-25.05", UseRegistries::All).unwrap();
        assert_eq!(
            resolved.to_url_text(false).unwrap(),
            "github:NixOS/nixpkgs/nixos-25.05"
        );
        // Exact entries do not match with extra ref/rev.
        assert!(lookup("flake:nix/master", UseRegistries::All).is_err());
        let (resolved, extra) = lookup("flake:nix", UseRegistries::All).unwrap();
        assert_eq!(resolved.to_url_text(false).unwrap(), "github:NixOS/nix");
        assert_eq!(extra.get("dir"), Some(&crate::Attr::String("sub".into())));
        // Chains resolve transitively.
        let (resolved, _) = lookup("flake:alias", UseRegistries::All).unwrap();
        assert_eq!(
            resolved.to_url_text(false).unwrap(),
            "github:NixOS/nixpkgs/nixpkgs-unstable"
        );
        assert!(
            lookup("flake:nixpkgs", UseRegistries::No).unwrap().0.scheme
                == Some(crate::Scheme::Indirect)
        );
        // Cycles are detected.
        let mut cyclic = reg.clone();
        let a = Input::from_url_str(&settings(), "flake:a", true).unwrap();
        let b = Input::from_url_str(&settings(), "flake:b", true).unwrap();
        for (from, to) in [(&a, &b), (&b, &a)] {
            cyclic.entries.push(Entry {
                from: from.clone(),
                to: to.clone(),
                extra_attrs: Attrs::new(),
                exact: false,
            });
        }
        let err = lookup_in_registries(&[cyclic], &a, UseRegistries::All).unwrap_err();
        assert!(err.to_string().starts_with("cycle detected"));
    }

    /// A registry is plain JSON data: no fetching is needed to resolve
    /// through it.
    #[test]
    fn a_global_registry_resolves_a_reference_with_a_ref() {
        let reg = parse(
            r#"{"version":2,"flakes":[{"from":{"type":"indirect","id":"nixpkgs"},"to":{"type":"github","owner":"NixOS","repo":"nixpkgs"}}]}"#,
            RegistryType::Global,
        );
        let input = Input::from_url_str(&settings(), "flake:nixpkgs/nixos-25.05", true).unwrap();
        let (resolved, _) = lookup_in_registries(&[reg], &input, UseRegistries::All).unwrap();
        assert_eq!(
            resolved.to_url_text(false).unwrap(),
            "github:NixOS/nixpkgs/nixos-25.05"
        );
    }

    /// What Nix does with registries it cannot understand: a warning, and
    /// the entries read before the problem.
    #[test]
    fn unreadable_registries_are_warnings() {
        let check = |text: &str, warning: &str, entries: usize| {
            let warnings = std::cell::RefCell::new(Vec::new());
            let warn = |w: &str| warnings.borrow_mut().push(w.to_owned());
            let reg = Registry::read_file(&settings(), "/r.json", text, RegistryType::User, &warn);
            assert_eq!(reg.entries.len(), entries, "{text}");
            let warnings = warnings.into_inner();
            assert_eq!(warnings.len(), 1, "{text}: {warnings:?}");
            assert!(warnings[0].starts_with(warning), "{text}: {warnings:?}");
        };
        check(
            r#"{"version":1}"#,
            "flake registry '/r.json' has unsupported version 1",
            0,
        );
        check(r#"{not json"#, "cannot parse flake registry '/r.json': ", 0);
        check(
            r#"{"version":"2"}"#,
            "cannot parse flake registry '/r.json': ",
            0,
        );
        // The first entry is kept, the second has a bad `exact`.
        check(
            r#"{"version":2,"flakes":[{"from":{"type":"indirect","id":"a"},"to":{"type":"path","path":"/x"}},{"from":{"type":"indirect","id":"b"},"to":{"type":"path","path":"/y"},"exact":"yes"}]}"#,
            "cannot parse flake registry '/r.json': ",
            1,
        );
        // An invalid input empties the registry.
        check(
            r#"{"version":2,"flakes":[{"from":{"type":"indirect","id":"a"},"to":{"type":"github","owner":"o","repo":"r","bogus":1}}]}"#,
            "cannot read flake registry '/r.json': error: input attribute 'bogus' not supported by scheme 'github'",
            0,
        );
        // No `flakes` at all is an empty registry, silently.
        assert!(parse(r#"{"version":2}"#, RegistryType::User)
            .entries
            .is_empty());
    }

    /// `UseRegistries::Limited` (flake inputs) consults only the flag and
    /// global registries, never the user's or the system's.
    #[test]
    fn limited_lookups_skip_user_and_system_registries() {
        let entry = |to: &str| {
            format!(
                r#"{{"version":2,"flakes":[{{"from":{{"type":"indirect","id":"x"}},"to":{{"type":"path","path":"{to}"}}}}]}}"#
            )
        };
        let registries = [
            parse(&entry("/user"), RegistryType::User),
            parse(&entry("/system"), RegistryType::System),
            parse(&entry("/global"), RegistryType::Global),
        ];
        let input = Input::from_url_str(&settings(), "flake:x", true).unwrap();
        let resolve = |mode| {
            lookup_in_registries(&registries, &input, mode)
                .unwrap()
                .0
                .to_url_text(false)
                .unwrap()
        };
        assert_eq!(resolve(UseRegistries::All), "path:/user");
        assert_eq!(resolve(UseRegistries::Limited), "path:/global");
        assert_eq!(
            lookup_in_registries(&registries[..2], &input, UseRegistries::Limited)
                .unwrap_err()
                .to_string(),
            "cannot find flake 'flake:x' in the flake registries"
        );
    }
}
