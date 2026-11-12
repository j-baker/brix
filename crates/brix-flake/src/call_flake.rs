//! The data `callFlake` feeds to Nix's `call-flake.nix`
//! (`src/libflake/flake.cc`, `src/libflake/call-flake.nix`,
//! `src/libexpr/primops/fetchTree.cc`'s `emitTreeAttrs`).
//!
//! `callFlake` evaluates `call-flake.nix` with two arguments:
//!
//! 1. `lockFileStr`: the lock file as JSON text, and
//! 2. `overrides`: an attribute set `{ <node key> = { sourceInfo; dir }; }`
//!    for every node whose tree is already fetched (always the root; also
//!    inputs that were fetched while locking).
//!
//! `call-flake.nix` then builds, for every node in the lock file:
//!
//! * `sourceInfo` = the override's `sourceInfo` if present; for a relative
//!   `path:` node the parent's `sourceInfo`; otherwise
//!   `builtins.fetchTree (removeAttrs node.locked ["dir"])`;
//! * `subdir` = the override's `dir`, else `node.locked.dir`, else `""`;
//! * `outPath` = for a relative node `parent.outPath + "/" + path`,
//!   otherwise `sourceInfo.outPath + "/" + subdir`;
//! * `flake` = `import (outPath + "/flake.nix")`;
//! * `inputs` = the node's inputs resolved (a `follows` list is looked up
//!   from the root) to their `result`;
//! * `outputs` = `flake.outputs (inputs // { self = result; })`;
//! * `result` = `outputs // sourceInfo // { inherit outPath inputs outputs
//!   sourceInfo; _type = "flake"; }` for flake nodes (after asserting that
//!   `flake.outputs` is a function), or `sourceInfo // { inherit sourceInfo
//!   outPath; }` for `flake = false` nodes.
//!
//! The value of the root node's `result` is the flake. The pieces that
//! need the evaluator (importing `flake.nix`, calling `outputs`) are the
//! facade's; this module produces the two arguments and the `sourceInfo`
//! attribute sets.

use std::collections::{BTreeMap, BTreeSet};

use crate::attrs::Attr;
use crate::flake::{FlakeDir, LockedFlake};
use crate::input::Input;
use crate::lockfile::{format_last_modified_date, Edge, LockFile, NodeId};
use crate::Result;

/// The attributes `emitTreeAttrs` produces for a fetched tree: what
/// `builtins.fetchTree` returns and what a flake's `sourceInfo` is.
/// Values are strings, integers or booleans; `outPath` is the store path
/// as a string (with its store-path string context in the evaluator).
pub type SourceInfo = BTreeMap<String, Attr>;

/// `emitTreeAttrs(storePath, input, emptyRevFallback, forceDirty)`.
///
/// `store_path` is the full path (`/nix/store/…-source`). With
/// `empty_rev_fallback` (only `builtins.fetchGit`), a tree without a
/// revision reports the all-zero revision and `revCount = 0`. With
/// `force_dirty`, `rev`/`revCount` are omitted even if known (a flake whose
/// lock file could not be written).
pub fn source_info(
    store_path: &str,
    input: &Input,
    empty_rev_fallback: bool,
    force_dirty: bool,
) -> Result<SourceInfo> {
    let mut attrs = SourceInfo::new();
    attrs.insert("outPath".into(), Attr::String(store_path.to_string()));

    // Without one (a lazily mounted tree, not hashed yet) Nix computes it
    // when it is asked for; the facade adds that lazily computed value.
    if let Some(h) = input.nar_hash()? {
        attrs.insert("narHash".into(), Attr::String(h.to_sri()));
    }

    if input.type_name()? == "git" {
        let submodules =
            crate::attrs::maybe_get_bool_attr(&input.attrs, "submodules")?.unwrap_or(false);
        attrs.insert("submodules".into(), Attr::Bool(submodules));
    }

    if !force_dirty {
        if let Some(rev) = input.rev()? {
            attrs.insert("rev".into(), Attr::String(rev.to_hex()));
            attrs.insert("shortRev".into(), Attr::String(rev.short()));
        } else if empty_rev_fallback {
            let null = crate::hashes::GitRev::NULL;
            attrs.insert("rev".into(), Attr::String(null.to_hex()));
            attrs.insert("shortRev".into(), Attr::String(null.short()));
        }
        if let Some(n) = input.rev_count()? {
            attrs.insert("revCount".into(), Attr::Int(n));
        } else if empty_rev_fallback {
            attrs.insert("revCount".into(), Attr::Int(0));
        }
    }

    if let Some(dirty_rev) = crate::attrs::maybe_get_str_attr(&input.attrs, "dirtyRev")? {
        attrs.insert("dirtyRev".into(), Attr::String(dirty_rev.to_string()));
        if let Some(short) = crate::attrs::maybe_get_str_attr(&input.attrs, "dirtyShortRev")? {
            attrs.insert("dirtyShortRev".into(), Attr::String(short.to_string()));
        }
    }

    if let Some(lm) = input.last_modified()? {
        attrs.insert("lastModified".into(), Attr::Int(lm));
        attrs.insert(
            "lastModifiedDate".into(),
            Attr::String(format_last_modified_date(lm)),
        );
    }

    Ok(attrs)
}

/// One entry of the `overrides` argument: a node whose tree is known.
#[derive(Clone, Debug)]
pub struct CallFlakeOverride<T> {
    /// The node's key in the lock file JSON (`root`, `nixpkgs`, …).
    pub node_key: String,
    /// The node in the lock file.
    pub node: NodeId,
    /// The tree and the subdirectory the flake lives in.
    pub dir: FlakeDir<T>,
    /// The locked input the `sourceInfo` is computed from.
    pub input: Input,
    /// `forceDirty` for `sourceInfo` (only ever true for the root).
    pub force_dirty: bool,
}

/// The two arguments of `call-flake.nix`.
#[derive(Clone, Debug)]
pub struct CallFlakeArgs<T> {
    /// `lockFileStr`.
    pub lock_file_json: String,
    /// `overrides`, one per fetched node. The facade turns each into
    /// `{ sourceInfo = <source_info(...)>; dir = <dir.subdir>; }`.
    pub overrides: Vec<CallFlakeOverride<T>>,
}

/// Which nodes to fetch in the background once evaluation first needs a
/// node's tree: a brix extension, since Nix fetches them one at a time.
///
/// `call-flake.nix` fetches a node's tree only when something forces it.
/// When that happens, a flake that declares the node is being evaluated
/// and is likely to need its other inputs too, and if the node is a flake
/// its own inputs come next. So the plan for a node lists the inputs of
/// every node that has it as an input, and its own inputs: a flake's
/// inputs are all fetched as soon as the first is needed, while the
/// inputs of a flake that evaluation never reaches are left alone, as Nix
/// leaves them.
///
/// Only nodes `call-flake.nix` fetches appear, as keys or in the lists:
/// not the root, not the nodes in `known` (trees fetched while locking,
/// passed as overrides), not relative `path:` inputs (part of their
/// parent's tree) and not build-time inputs (a derivation that fetches
/// them when it is built).
pub fn prefetch_plan(lock: &LockFile, known: &BTreeSet<NodeId>) -> BTreeMap<NodeId, Vec<NodeId>> {
    let fetched = |id: NodeId| {
        !known.contains(&id)
            && lock
                .node(id)
                .locked
                .as_ref()
                .is_some_and(|l| !l.build_time && l.locked_ref.input.is_relative().is_none())
    };
    // Each node's inputs, `follows` resolved.
    let inputs: Vec<BTreeSet<NodeId>> = (0..lock.nodes.len())
        .map(|i| {
            lock.node(NodeId(i))
                .inputs
                .values()
                .filter_map(|edge| match edge {
                    Edge::Node(child) => Some(*child),
                    Edge::Follows(path) => lock.find_input(path).ok().flatten(),
                })
                .collect()
        })
        .collect();
    let mut plan: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for siblings in &inputs {
        for &node in siblings.iter().filter(|&&n| fetched(n)) {
            let entry = plan.entry(node).or_default();
            entry.extend(siblings);
            entry.extend(&inputs[node.0]);
        }
    }
    plan.into_iter()
        .map(|(node, others)| {
            let others: Vec<NodeId> = others
                .into_iter()
                .filter(|&n| n != node && fetched(n))
                .collect();
            (node, others)
        })
        .filter(|(_, others)| !others.is_empty())
        .collect()
}

/// `callFlake`'s preparation: everything but the evaluation.
pub fn call_flake_args<T: Clone>(locked: &LockedFlake<T>) -> CallFlakeArgs<T> {
    let (json, keys) = locked.lock_file.to_json();
    let lock_file_json = crate::lockfile::pretty_json(&json);
    let mut overrides = Vec::new();
    for (node_id, dir) in &locked.node_paths {
        let node = locked.lock_file.node(*node_id);
        let (input, force_dirty) = match &node.locked {
            Some(l) => (l.locked_ref.input.clone(), false),
            None => (
                locked.flake.locked_ref.input.clone(),
                locked.flake.force_dirty,
            ),
        };
        overrides.push(CallFlakeOverride {
            node_key: keys[node_id].clone(),
            node: *node_id,
            dir: dir.clone(),
            input,
            force_dirty,
        });
    }
    CallFlakeArgs {
        lock_file_json,
        overrides,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;
    use crate::input::FetchSettings;

    #[test]
    fn source_info_for_github_tree() {
        let input = Input::from_attrs(
            &FetchSettings::default(),
            attrs([
                ("type", "github".into()),
                ("owner", "NixOS".into()),
                ("repo", "nixpkgs".into()),
                ("rev", "addf7cf5f383a3101ecfba091b98d0a1263dc9b8".into()),
                (
                    "narHash",
                    "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into(),
                ),
                ("lastModified", 1765186076u64.into()),
            ]),
        )
        .unwrap();
        let info = source_info("/nix/store/x-source", &input, false, false).unwrap();
        assert_eq!(
            info,
            [
                ("lastModified", Attr::Int(1765186076)),
                ("lastModifiedDate", Attr::String("20251208092756".into())),
                (
                    "narHash",
                    Attr::String("sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into())
                ),
                ("outPath", Attr::String("/nix/store/x-source".into())),
                (
                    "rev",
                    Attr::String("addf7cf5f383a3101ecfba091b98d0a1263dc9b8".into())
                ),
                ("shortRev", Attr::String("addf7cf".into())),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
        );
        let dirty = source_info("/nix/store/x-source", &input, false, true).unwrap();
        assert!(!dirty.contains_key("rev"));
    }

    #[test]
    fn fetch_git_fallback_reports_null_rev() {
        let input = Input::from_attrs(
            &FetchSettings::default(),
            attrs([("type", "git".into()), ("url", "https://x/y".into())]),
        )
        .unwrap();
        let info = source_info("/nix/store/x-source", &input, true, false).unwrap();
        assert_eq!(info["rev"], Attr::String("0".repeat(40)));
        assert_eq!(info["shortRev"], Attr::String("0000000".into()));
        assert_eq!(info["revCount"], Attr::Int(0));
        assert_eq!(info["submodules"], Attr::Bool(false));
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::input::FetchSettings;

    fn github(repo: &str, rev: char) -> String {
        format!(
            r#""locked": {{ "lastModified": 1, "narHash": "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=", "owner": "x", "repo": "{repo}", "rev": "{}", "type": "github" }}, "original": {{ "owner": "x", "repo": "{repo}", "type": "github" }}"#,
            rev.to_string().repeat(40)
        )
    }

    /// root → a (inputs x, and y following b), b, rel (relative), bt
    /// (build-time).
    fn lock() -> LockFile {
        let text = format!(
            r#"{{
  "nodes": {{
    "a": {{ {a}, "inputs": {{ "x": "x", "y": ["b"] }} }},
    "b": {{ {b} }},
    "bt": {{ {bt}, "buildTime": true }},
    "rel": {{ "locked": {{ "path": "./sub", "type": "path" }}, "original": {{ "path": "./sub", "type": "path" }}, "parent": [] }},
    "root": {{ "inputs": {{ "a": "a", "b": "b", "bt": "bt", "rel": "rel" }} }},
    "x": {{ {x} }}
  }},
  "root": "root",
  "version": 7
}}"#,
            a = github("a", 'a'),
            b = github("b", 'b'),
            bt = github("bt", 'c'),
            x = github("x", 'd'),
        );
        LockFile::parse(&FetchSettings::default(), &text, "flake.lock").unwrap()
    }

    /// The plan by node key, for readable assertions.
    fn named(lock: &LockFile, known: &[&str]) -> BTreeMap<String, Vec<String>> {
        let (_, keys) = lock.to_json();
        let id = |name: &str| *keys.iter().find(|(_, k)| *k == name).unwrap().0;
        let known: BTreeSet<NodeId> = known.iter().map(|k| id(k)).collect();
        prefetch_plan(lock, &known)
            .into_iter()
            .map(|(node, others)| {
                let mut others: Vec<String> = others.iter().map(|n| keys[n].clone()).collect();
                others.sort();
                (keys[&node].clone(), others)
            })
            .collect()
    }

    #[test]
    fn a_needed_node_brings_its_siblings_and_its_own_inputs() {
        let lock = lock();
        let plan = named(&lock, &[]);
        let expected: BTreeMap<String, Vec<String>> = [
            // The root's other inputs, and a's own (y follows b).
            ("a", vec!["b", "x"]),
            // An input of the root and (through `follows`) of a.
            ("b", vec!["a", "x"]),
            ("x", vec!["b"]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
        .collect();
        assert_eq!(plan, expected);

        // A node fetched while locking is never fetched again.
        let plan = named(&lock, &["a"]);
        assert_eq!(plan["b"], ["x"]);
        assert!(!plan.contains_key("a"));
    }
}
