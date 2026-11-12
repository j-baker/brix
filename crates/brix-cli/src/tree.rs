//! The command tree, built from the inventory of the real `nix` CLI.
//!
//! `data/nix-cli-tree.json` is `nix __dump-cli` with every description and
//! document stripped (regenerate it with `scripts/update-cli-tree.sh`).
//! Loading it gives brix exactly `nix`'s subcommands, flags, arities,
//! labels and categories; the wording comes from [`crate::descriptions`],
//! and a few hidden `__…` commands and the flag aliases `nix` does not
//! dump are added here.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::args::{Alias, Arg, Arity, Category, Command, Flag};
use crate::descriptions;

const INVENTORY: &str = include_str!("../data/nix-cli-tree.json");

/// The whole tree. The root command has the empty name.
pub fn build() -> Command {
    let inventory: InventoryCommand = serde_json::from_str(INVENTORY)
        .expect("data/nix-cli-tree.json is `nix __dump-cli`'s command tree");
    let mut root = from_inventory("", "", inventory);
    add_brix_extensions(&mut root);
    root.commands.push(hidden("__dump-cli", vec![]));
    root.commands.push(hidden(
        "__parse",
        vec![Arg {
            label: "file".into(),
            optional: false,
            arity: Arity::Fixed(1),
        }],
    ));
    add_aliases(&mut root);
    root
}

/// The flags brix adds to `nix`'s tree ([`add_brix_extensions`]). Nix
/// rejects them, so the commands brix hands to Nix refuse them.
pub const BRIX_ONLY_FLAGS: [&str; 2] = ["compiled-input", "no-compiled-inputs"];

/// The command and flags brix adds to `nix`'s tree, for compiled
/// flake-input images (`brix_eval::image`): `flake compile` and the
/// [`BRIX_ONLY_FLAGS`]. They are marked as brix extensions in their help
/// and the inventory test lists them explicitly. The only other departure
/// from the inventory is the hidden development aid `__parse` (see
/// [`build`]), which `__dump-cli` leaves out as it does Nix's hidden
/// commands.
fn add_brix_extensions(root: &mut Command) {
    const COMPILE: &str = "flake compile";
    // `path` is the command the flag belongs to, which its description
    // can depend on.
    let flag = |path: &str, long: &str, labels: &[&str], arity: usize| Flag {
        long: long.to_owned(),
        short: None,
        aliases: Vec::new(),
        labels: labels.iter().map(|s| (*s).to_owned()).collect(),
        arity: Arity::Fixed(arity),
        category: String::new(),
        hidden_category: false,
        experimental: None,
        description: descriptions::flag(path, long).to_owned(),
    };
    let [compiled_input, no_compiled_inputs] = BRIX_ONLY_FLAGS;
    root.flags
        .push(flag("", compiled_input, &["name-or-hash=path"], 1));
    root.flags.push(flag("", no_compiled_inputs, &[], 0));
    root.flags.sort_by(|a, b| a.long.cmp(&b.long));
    let flake = root
        .commands
        .iter_mut()
        .find(|c| c.name == "flake")
        .expect("nix has a flake command");
    let mut compile = Command {
        name: "compile".to_owned(),
        description: descriptions::command(COMPILE).to_owned(),
        hidden: false,
        experimental: None,
        // The category of `flake`'s other subcommands.
        category: Category {
            id: 0,
            description: category_name(0, COMPILE),
        },
        flags: vec![
            flag(COMPILE, "input", &["input-name"], 1),
            flag(COMPILE, "out", &["path"], 1),
            flag(COMPILE, "no-self", &[], 0),
            flag(COMPILE, "with-tree", &[], 0),
            flag(COMPILE, "json", &[], 0),
        ],
        args: vec![Arg {
            label: "flake-url".into(),
            optional: true,
            arity: Arity::Fixed(1),
        }],
        commands: Vec::new(),
    };
    compile.flags.sort_by(|a, b| a.long.cmp(&b.long));
    flake.commands.push(compile);
    flake.commands.sort_by(|a, b| a.name.cmp(&b.name));
}

/// `nix`'s top-level aliases (`main.cc`): old command names that are
/// rewritten to their new form.
pub fn aliases() -> Vec<Alias> {
    let deprecated = [
        ("add-to-store", &["store", "add-path"][..]),
        ("cat-nar", &["nar", "cat"]),
        ("cat-store", &["store", "cat"]),
        ("copy-sigs", &["store", "copy-sigs"]),
        ("dev-shell", &["develop"]),
        ("diff-closures", &["store", "diff-closures"]),
        ("dump-path", &["store", "dump-path"]),
        ("hash-file", &["hash", "file"]),
        ("hash-path", &["hash", "path"]),
        ("ls-nar", &["nar", "ls"]),
        ("ls-store", &["store", "ls"]),
        (
            "make-content-addressable",
            &["store", "make-content-addressed"],
        ),
        ("optimise-store", &["store", "optimise"]),
        ("ping-store", &["store", "info"]),
        ("realisation", &["store", "build-trace"]),
        ("sign-paths", &["store", "sign"]),
        ("show-derivation", &["derivation", "show"]),
        ("show-config", &["config", "show"]),
        ("to-base16", &["hash", "to-base16"]),
        ("to-base32", &["hash", "to-base32"]),
        ("to-base64", &["hash", "to-base64"]),
        ("verify", &["store", "verify"]),
        ("doctor", &["config", "check"]),
    ];
    let mut out: Vec<Alias> = deprecated
        .iter()
        .map(|(name, replacement)| Alias {
            name: (*name).to_owned(),
            replacement: replacement.iter().map(|s| (*s).to_owned()).collect(),
            deprecated: true,
        })
        .collect();
    out.push(Alias {
        name: "shell".into(),
        replacement: vec!["env".into(), "shell".into()],
        deprecated: false,
    });
    out
}

/// A command of the inventory, as `MultiCommand::toJSON` writes it (less
/// the descriptions the generator strips). Unknown fields are an error,
/// so that a change in Nix's format is noticed rather than misread.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryCommand {
    args: Vec<InventoryArg>,
    flags: BTreeMap<String, InventoryFlag>,
    /// A multi-command's subcommands.
    #[serde(default)]
    commands: BTreeMap<String, InventoryCommand>,
    /// Every command's but the root's.
    category: Option<InventoryCategory>,
}

/// A positional argument (`Args::toJSON`); no `arity` means any number.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryArg {
    label: String,
    optional: bool,
    arity: Option<usize>,
}

/// A flag (`Args::Flag` in `Args::toJSON`); no `arity` means any number.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InventoryFlag {
    arity: Option<usize>,
    category: String,
    #[serde(rename = "experimental-feature")]
    experimental_feature: Option<String>,
    hidden_category: bool,
    #[serde(default)]
    labels: Vec<String>,
    short_name: Option<char>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryCategory {
    id: i64,
    #[serde(rename = "experimental-feature")]
    experimental_feature: Option<String>,
}

fn arity(n: Option<usize>) -> Arity {
    n.map_or(Arity::Any, Arity::Fixed)
}

/// The command at `path` (`""` for the root) named `name`, with its
/// subcommands, and brix's wording for it.
fn from_inventory(path: &str, name: &str, cmd: InventoryCommand) -> Command {
    let flags = cmd
        .flags
        .into_iter()
        .map(|(long, f)| {
            let description = if f.category == SETTINGS_CATEGORY {
                descriptions::setting(&long)
            } else {
                descriptions::flag(path, &long).to_owned()
            };
            Flag {
                long,
                short: f.short_name,
                aliases: Vec::new(),
                labels: f.labels,
                arity: arity(f.arity),
                category: f.category,
                hidden_category: f.hidden_category,
                experimental: f.experimental_feature,
                description,
            }
        })
        .collect();
    let args = cmd
        .args
        .into_iter()
        .map(|a| Arg {
            label: a.label,
            optional: a.optional,
            arity: arity(a.arity),
        })
        .collect();
    let commands = cmd
        .commands
        .into_iter()
        .map(|(sub, j)| {
            let sub_path = if path.is_empty() {
                sub.clone()
            } else {
                format!("{path} {sub}")
            };
            from_inventory(&sub_path, &sub, j)
        })
        .collect();
    let (category, experimental) = match cmd.category {
        Some(c) => (
            Category {
                id: c.id,
                description: category_name(c.id, path),
            },
            c.experimental_feature,
        ),
        None => (
            Category {
                id: 0,
                description: String::new(),
            },
            None,
        ),
    };
    Command {
        name: name.to_owned(),
        description: descriptions::command(path).to_owned(),
        hidden: false,
        experimental,
        category,
        flags,
        args,
        commands,
    }
}

/// The category flags for `nix.conf` settings carry.
pub const SETTINGS_CATEGORY: &str = "Options to override configuration settings";

/// `nix`'s category names: the root groups its commands into several
/// categories, subcommand groups have a single one.
fn category_name(id: i64, path: &str) -> String {
    let top_level = !path.contains(' ');
    match (id, top_level) {
        (-1, true) => "Help commands",
        (0, true) => "Main commands",
        (0, false) => "Available commands",
        (100, true) => "Infrequently used commands",
        (101, true) => "Utility/scripting commands",
        (102, true) => "Commands for upgrading or troubleshooting your Nix installation",
        // A group (`NixMultiCommand`) names only its default category.
        _ => "",
    }
    .to_owned()
}

fn hidden(name: &str, args: Vec<Arg>) -> Command {
    Command {
        name: name.into(),
        description: descriptions::command(name).to_owned(),
        hidden: true,
        experimental: None,
        category: Category {
            id: 103,
            description: "Undocumented commands".into(),
        },
        flags: Vec::new(),
        args,
        commands: Vec::new(),
    }
}

/// Flag aliases `nix` accepts but does not list in `__dump-cli`.
fn add_aliases(root: &mut Command) {
    fn add_alias_to(cmd: &mut Command, long: &str, alias: &str) {
        if let Some(f) = cmd.flags.iter_mut().find(|f| f.long == long) {
            f.aliases.push(alias.to_owned());
        }
        for sub in &mut cmd.commands {
            add_alias_to(sub, long, alias);
        }
    }
    add_alias_to(root, "offline", "no-net");
    add_alias_to(root, "ignore-env", "ignore-environment");
    add_alias_to(root, "keep-env-var", "keep");
    add_alias_to(root, "unset-env-var", "unset");
    // A setting's aliases are flags too: `convertToArg` gives every flag
    // it adds for the setting the setting's aliases, and `Args::addFlag`
    // lets a later flag take a name over, so an alias is the last of them:
    // `--no-NAME` for a Boolean setting (`--build-fallback` turns
    // `fallback` off), `--relaxed-sandbox` for `sandbox`, `--extra-NAME`
    // for a list setting (`--binary-caches` appends to `substituters`),
    // and `--NAME` for the others.
    for &(alias, setting) in brix::nixconf::SETTING_ALIASES {
        let flag = ["relaxed-", "no-", "extra-", ""]
            .iter()
            .map(|prefix| format!("{prefix}{setting}"))
            .find(|flag| root.flags.iter().any(|f| f.long == *flag))
            .unwrap_or_else(|| panic!("the setting '{setting}' of the alias '{alias}' has a flag"));
        add_alias_to(root, &flag, alias);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_has_nix_structure() {
        let root = build();
        let show = root
            .descend(&["flake".into(), "show".into()])
            .expect("flake show exists");
        assert_eq!(show.description, "show the outputs provided by a flake");
        assert!(show.flags.iter().any(|f| f.long == "legacy"));
        assert_eq!(show.args[0].label, "flake-url");
        let build = root.subcommand("build").unwrap();
        let out_link = build.flags.iter().find(|f| f.long == "out-link").unwrap();
        assert_eq!(out_link.short, Some('o'));
        assert_eq!(out_link.labels, vec!["path"]);
        assert!(root.subcommand("__dump-cli").unwrap().hidden);
        assert!(root
            .flags
            .iter()
            .any(|f| f.long == "option" && f.arity == Arity::Fixed(2)));
    }

    #[test]
    fn every_flag_has_a_description() {
        fn check(cmd: &Command, path: &str, missing: &mut Vec<String>) {
            for flag in &cmd.flags {
                if flag.description.is_empty() {
                    missing.push(format!("{path} --{}", flag.long));
                }
            }
            for sub in &cmd.commands {
                check(sub, &format!("{path} {}", sub.name), missing);
            }
        }
        let mut missing = Vec::new();
        check(&build(), "brix", &mut missing);
        assert!(
            missing.is_empty(),
            "flags without a description: {missing:?}"
        );
        // brix's own flags get the wording of the command they are on.
        let root = build();
        let compile = root
            .descend(&["flake".into(), "compile".into()])
            .expect("flake compile exists");
        let input = compile.flags.iter().find(|f| f.long == "input").unwrap();
        assert_eq!(
            input.description,
            descriptions::flag("flake compile", "input")
        );
        assert_eq!(
            compile.category,
            root.subcommand("flake")
                .unwrap()
                .subcommand("show")
                .unwrap()
                .category
        );
    }

    #[test]
    fn the_inventory_shape_is_checked() {
        let unknown = r#"{"args": [], "flags": {}, "deprecated": true}"#;
        assert!(serde_json::from_str::<InventoryCommand>(unknown).is_err());
        let missing = r#"{"args": [{"label": "x"}], "flags": {}}"#;
        assert!(serde_json::from_str::<InventoryCommand>(missing).is_err());
        let any = r#"{"args": [{"label": "x", "optional": true}], "flags": {}}"#;
        let cmd = from_inventory("", "", serde_json::from_str(any).unwrap());
        assert_eq!(cmd.args[0].arity, Arity::Any);
    }

    #[test]
    fn every_listed_command_has_a_description() {
        fn check(cmd: &Command, path: &str, missing: &mut Vec<String>) {
            if cmd.description.is_empty() {
                missing.push(path.to_owned());
            }
            for sub in &cmd.commands {
                let p = if path.is_empty() {
                    sub.name.clone()
                } else {
                    format!("{path} {}", sub.name)
                };
                check(sub, &p, missing);
            }
        }
        let mut missing = Vec::new();
        check(&build(), "", &mut missing);
        assert!(
            missing.is_empty(),
            "commands without a description: {missing:?}"
        );
    }
}
