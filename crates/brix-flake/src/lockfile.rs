//! `flake.lock` (`src/libflake/lockfile.cc`).
//!
//! A lock file is a graph: a root node whose inputs are either other nodes
//! (locked inputs) or *follows* paths pointing elsewhere in the graph. Nix
//! reads versions 5 to 7 and always writes version 7. Serialisation is
//! byte-identical to Nix's: nlohmann's `dump(2)` with sorted keys, which
//! `serde_json`'s pretty printer reproduces once the maps are `BTreeMap`s.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{json, Map, Value};

use crate::attrs::{attrs_to_json, json_to_attrs};
use crate::flakeref::FlakeRef;
use crate::input::{is_flake_id, FetchSettings};
use crate::{err, Error, Result};

/// `InputAttrPath`: the path of an input from the root, e.g.
/// `["dwarffs", "nixpkgs"]`.
pub type InputAttrPath = Vec<String>;

/// `parseInputAttrPath`: `a/b/c`.
pub fn parse_input_attr_path(s: &str) -> Result<InputAttrPath> {
    let mut path = Vec::new();
    for elem in s.split('/').filter(|e| !e.is_empty()) {
        if !is_flake_id(elem) {
            return Err(err!(
                "invalid flake input attribute path element '{}'",
                elem
            ));
        }
        path.push(elem.to_string());
    }
    Ok(path)
}

/// `printInputAttrPath`.
pub fn print_input_attr_path(path: &[String]) -> String {
    path.join("/")
}

/// Index of a node in [`LockFile::nodes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub usize);

/// `Node::Edge`: how an input of a node is resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    /// A locked node of its own.
    Node(NodeId),
    /// `follows`: the input at this path from the root.
    Follows(InputAttrPath),
}

/// A node in the lock graph. The root has no `locked` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub inputs: BTreeMap<String, Edge>,
    /// `None` for the root node (`Node`), `Some` for `LockedNode`.
    pub locked: Option<LockedNode>,
}

/// The lock-specific part of `LockedNode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedNode {
    pub locked_ref: FlakeRef,
    pub original_ref: FlakeRef,
    pub is_flake: bool,
    pub build_time: bool,
    /// For relative `path:` inputs: the node relative to which the path
    /// is interpreted.
    pub parent_input_attr_path: Option<InputAttrPath>,
}

/// `LockFile`.
#[derive(Clone, Debug)]
pub struct LockFile {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

/// `LockFile::operator==`: two lock files are equal when their JSON is,
/// whatever the numbering of their nodes (and nodes no input reaches).
impl PartialEq for LockFile {
    fn eq(&self, other: &LockFile) -> bool {
        self.to_json().0 == other.to_json().0
    }
}

impl Eq for LockFile {}

impl Default for LockFile {
    fn default() -> Self {
        LockFile::new()
    }
}

impl LockFile {
    /// An empty lock file: a root with no inputs.
    pub fn new() -> LockFile {
        LockFile {
            nodes: vec![Node {
                inputs: BTreeMap::new(),
                locked: None,
            }],
            root: NodeId(0),
        }
    }

    /// Add a node and return its id.
    pub fn add_node(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        NodeId(self.nodes.len() - 1)
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    /// `LockFile::LockFile(contents, path)`: parse a lock file. `path` is
    /// only used in error messages.
    pub fn parse(settings: &FetchSettings, contents: &str, path: &str) -> Result<LockFile> {
        let json: Value = serde_json::from_str(contents)
            .map_err(|e| err!("Could not parse '{}': {}", path, e))?;
        let version = json.get("version").and_then(Value::as_i64).unwrap_or(0);
        if !(5..=7).contains(&version) {
            return Err(err!(
                "lock file '{}' has unsupported version {}",
                path,
                version
            ));
        }
        let root_key = json
            .get("root")
            .and_then(Value::as_str)
            .ok_or_else(|| err!("lock file '{}' has no root node", path))?
            .to_string();
        let nodes_json = json
            .get("nodes")
            .and_then(Value::as_object)
            .ok_or_else(|| err!("lock file '{}' has no nodes", path))?;

        let mut lock = LockFile::new();
        let mut node_map: BTreeMap<String, NodeId> = BTreeMap::new();
        node_map.insert(root_key.clone(), lock.root);
        let root_json = nodes_json
            .get(&root_key)
            .ok_or_else(|| err!("lock file references missing node '{}'", root_key))?;
        lock.read_inputs(settings, lock.root, root_json, nodes_json, &mut node_map)?;
        Ok(lock)
    }

    fn read_inputs(
        &mut self,
        settings: &FetchSettings,
        node: NodeId,
        json_node: &Value,
        nodes_json: &Map<String, Value>,
        node_map: &mut BTreeMap<String, NodeId>,
    ) -> Result<()> {
        let Some(inputs) = json_node.get("inputs").and_then(Value::as_object) else {
            return Ok(());
        };
        for (name, value) in inputs {
            let edge = match value {
                Value::Array(items) => {
                    let mut path = Vec::new();
                    for item in items {
                        path.push(
                            item.as_str()
                                .ok_or_else(|| err!("invalid 'follows' path in lock file"))?
                                .to_string(),
                        );
                    }
                    Edge::Follows(path)
                }
                Value::String(input_key) => {
                    let id = match node_map.get(input_key) {
                        Some(id) => *id,
                        None => {
                            let json_node2 = nodes_json.get(input_key).ok_or_else(|| {
                                err!("lock file references missing node '{}'", input_key)
                            })?;
                            let locked = LockedNode::from_json(settings, json_node2)?;
                            let id = self.add_node(Node {
                                inputs: BTreeMap::new(),
                                locked: Some(locked),
                            });
                            node_map.insert(input_key.clone(), id);
                            self.read_inputs(settings, id, json_node2, nodes_json, node_map)?;
                            id
                        }
                    };
                    if self.node(id).locked.is_none() {
                        return Err(err!("lock file contains cycle to root node"));
                    }
                    Edge::Node(id)
                }
                _ => return Err(err!("invalid input in lock file")),
            };
            self.node_mut(node).inputs.insert(name.clone(), edge);
        }
        Ok(())
    }

    /// `LockFile::toJSON`: the JSON document and the key assigned to each
    /// node. Keys are the input names, with `_2`, `_3`… suffixes when
    /// several nodes share one, assigned in traversal order from the root.
    pub fn to_json(&self) -> (Value, BTreeMap<NodeId, String>) {
        let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
        let mut node_keys: BTreeMap<NodeId, String> = BTreeMap::new();
        let mut keys: BTreeSet<String> = BTreeSet::new();
        let root_key = self.dump_node("root", self.root, &mut nodes, &mut node_keys, &mut keys);
        let json = json!({
            "nodes": Value::Object(nodes.into_iter().collect()),
            "root": root_key,
            "version": 7,
        });
        (json, node_keys)
    }

    fn dump_node(
        &self,
        key: &str,
        id: NodeId,
        nodes: &mut BTreeMap<String, Value>,
        node_keys: &mut BTreeMap<NodeId, String>,
        keys: &mut BTreeSet<String>,
    ) -> String {
        if let Some(k) = node_keys.get(&id) {
            return k.clone();
        }
        let mut key = key.to_string();
        if !keys.insert(key.clone()) {
            for n in 2.. {
                let k = format!("{key}_{n}");
                if keys.insert(k.clone()) {
                    key = k;
                    break;
                }
            }
        }
        node_keys.insert(id, key.clone());

        // nlohmann prints object keys sorted; build the node in that order.
        let node = self.node(id);
        let mut n: BTreeMap<String, Value> = BTreeMap::new();
        if !node.inputs.is_empty() {
            let mut inputs: BTreeMap<String, Value> = BTreeMap::new();
            for (name, edge) in &node.inputs {
                let v = match edge {
                    Edge::Node(child) => {
                        Value::String(self.dump_node(name, *child, nodes, node_keys, keys))
                    }
                    Edge::Follows(path) => {
                        Value::Array(path.iter().map(|p| Value::String(p.clone())).collect())
                    }
                };
                inputs.insert(name.clone(), v);
            }
            n.insert("inputs".into(), Value::Object(inputs.into_iter().collect()));
        }
        if let Some(locked) = &node.locked {
            n.insert(
                "original".into(),
                attrs_to_json(&locked.original_ref.to_attrs()),
            );
            n.insert(
                "locked".into(),
                attrs_to_json(&locked.locked_ref.to_attrs()),
            );
            if !locked.is_flake {
                n.insert("flake".into(), Value::Bool(false));
            }
            if locked.build_time {
                n.insert("buildTime".into(), Value::Bool(true));
            }
            if let Some(parent) = &locked.parent_input_attr_path {
                n.insert(
                    "parent".into(),
                    Value::Array(parent.iter().map(|p| Value::String(p.clone())).collect()),
                );
            }
        }
        nodes.insert(key.clone(), Value::Object(n.into_iter().collect()));
        key
    }

    /// `LockFile::to_string`: `json.dump(2)`, without a trailing newline
    /// (Nix adds one when writing the file).
    pub fn to_json_string(&self) -> String {
        let (json, _) = self.to_json();
        pretty_json(&json)
    }

    /// `LockFile::isUnlocked`: the first input (if any) that is not locked
    /// and so would make evaluation irreproducible. With
    /// `allow-dirty-locks`, a `narHash` is enough.
    pub fn is_unlocked(&self, settings: &FetchSettings) -> Result<Option<&FlakeRef>> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            for edge in self.node(id).inputs.values() {
                if let Edge::Node(child) = edge {
                    stack.push(*child);
                }
            }
        }
        for id in seen {
            if id == self.root {
                continue;
            }
            let Some(node) = &self.node(id).locked else {
                continue;
            };
            let input = &node.locked_ref.input;
            let considered_locked = input.is_locked(settings)?
                || (settings.allow_dirty_locks && input.nar_hash()?.is_some());
            if (!considered_locked || !input.is_final()) && input.is_relative().is_none() {
                return Ok(Some(&node.locked_ref));
            }
        }
        Ok(None)
    }

    /// `LockFile::findInput`: follow a path from the root, resolving
    /// `follows` edges, and detect follow cycles.
    pub fn find_input(&self, path: &[String]) -> Result<Option<NodeId>> {
        let mut visited = Vec::new();
        self.do_find(path, &mut visited)
    }

    fn do_find(&self, path: &[String], visited: &mut Vec<InputAttrPath>) -> Result<Option<NodeId>> {
        if let Some(pos) = visited.iter().position(|p| p == path) {
            let mut cycle: Vec<String> = visited[pos..]
                .iter()
                .map(|p| print_input_attr_path(p))
                .collect();
            cycle.push(print_input_attr_path(path));
            return Err(err!("follow cycle detected: [{}]", cycle.join(" -> ")));
        }
        visited.push(path.to_vec());
        let mut pos = self.root;
        for elem in path {
            match self.node(pos).inputs.get(elem) {
                Some(Edge::Node(child)) => pos = *child,
                Some(Edge::Follows(follows)) => match self.do_find(follows, visited)? {
                    Some(p) => pos = p,
                    None => return Ok(None),
                },
                None => return Ok(None),
            }
        }
        Ok(Some(pos))
    }

    /// `LockFile::getAllInputs`: every edge reachable from the root, keyed
    /// by its attribute path.
    pub fn all_inputs(&self) -> BTreeMap<InputAttrPath, Edge> {
        let mut done = BTreeSet::new();
        let mut res = BTreeMap::new();
        self.collect_inputs(&[], self.root, &mut done, &mut res);
        res
    }

    fn collect_inputs(
        &self,
        prefix: &[String],
        id: NodeId,
        done: &mut BTreeSet<NodeId>,
        res: &mut BTreeMap<InputAttrPath, Edge>,
    ) {
        if !done.insert(id) {
            return;
        }
        for (name, edge) in &self.node(id).inputs {
            let mut path = prefix.to_vec();
            path.push(name.clone());
            res.insert(path.clone(), edge.clone());
            if let Edge::Node(child) = edge {
                self.collect_inputs(&path, *child, done, res);
            }
        }
    }

    /// `LockFile::check`: every `follows` must point at an existing input.
    pub fn check(&self) -> Result<()> {
        for (path, edge) in self.all_inputs() {
            if let Edge::Follows(follows) = edge {
                if !follows.is_empty() && self.find_input(&follows)?.is_none() {
                    return Err(err!(
                        "input '{}' follows a non-existent input '{}'",
                        print_input_attr_path(&path),
                        print_input_attr_path(&follows)
                    ));
                }
            }
        }
        Ok(())
    }

    /// `LockFile::diff`: a human-readable summary of added, removed and
    /// updated inputs (without Nix's ANSI colours).
    pub fn diff(old: &LockFile, new: &LockFile) -> String {
        let old_flat = old.all_inputs();
        let new_flat = new.all_inputs();
        let describe = |lock: &LockFile, edge: &Edge| match edge {
            Edge::Node(id) => {
                let node = lock.node(*id).locked.as_ref();
                match node {
                    Some(n) => {
                        let mut s =
                            format!("'{}'", n.locked_ref.to_url_string(true).unwrap_or_default());
                        if let Ok(Some(lm)) = n.locked_ref.input.last_modified() {
                            s.push_str(&format!(" ({})", format_date(lm)));
                        }
                        s
                    }
                    None => "root".to_string(),
                }
            }
            Edge::Follows(f) => format!("follows '{}'", print_input_attr_path(f)),
        };
        let equals = |e1: &Edge, e2: &Edge| match (e1, e2) {
            (Edge::Node(a), Edge::Node(b)) => {
                old.node(*a).locked.as_ref().map(|n| &n.locked_ref)
                    == new.node(*b).locked.as_ref().map(|n| &n.locked_ref)
            }
            (Edge::Follows(a), Edge::Follows(b)) => a == b,
            _ => false,
        };
        let mut res = String::new();
        let mut i = old_flat.iter().peekable();
        let mut j = new_flat.iter().peekable();
        loop {
            match (i.peek(), j.peek()) {
                (None, None) => break,
                (oi, Some((jp, je))) if oi.is_none_or(|(ip, _)| *ip > *jp) => {
                    res.push_str(&format!(
                        "• Added input '{}':\n    {}\n",
                        print_input_attr_path(jp),
                        describe(new, je)
                    ));
                    j.next();
                }
                (Some((ip, _)), oj) if oj.is_none_or(|(jp, _)| *ip < *jp) => {
                    res.push_str(&format!(
                        "• Removed input '{}'\n",
                        print_input_attr_path(ip)
                    ));
                    i.next();
                }
                (Some((ip, ie)), Some((_, je))) => {
                    if !equals(ie, je) {
                        res.push_str(&format!(
                            "• Updated input '{}':\n    {}\n  → {}\n",
                            print_input_attr_path(ip),
                            describe(old, ie),
                            describe(new, je)
                        ));
                    }
                    i.next();
                    j.next();
                }
                _ => unreachable!(),
            }
        }
        res
    }
}

impl fmt::Display for LockFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

impl LockedNode {
    /// `LockedNode::LockedNode(json)`: one `nodes` entry. Entries are
    /// implicitly final. An entry whose input is not locked is rejected,
    /// unless it carries a `narHash` (then Nix only warns).
    pub fn from_json(settings: &FetchSettings, json: &Value) -> Result<LockedNode> {
        let get_ref = |attr: &str, info: Option<&str>| -> Result<FlakeRef> {
            let Some(v) = json.get(attr) else {
                return Err(err!("attribute '{}' missing in lock file", attr));
            };
            let mut attrs = json_to_attrs(v)?;
            // Lock file version 5 kept some attributes in `info`.
            if let Some(info) = info.and_then(|i| json.get(i)) {
                for (k, v) in json_to_attrs(info)? {
                    attrs.insert(k, v);
                }
            }
            FlakeRef::from_attrs(settings, &attrs)
        };
        let mut locked_ref = get_ref("locked", Some("info"))?;
        let original_ref = get_ref("original", None)?;
        let is_flake = json.get("flake").and_then(Value::as_bool).unwrap_or(true);
        let build_time = json
            .get("buildTime")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let parent_input_attr_path = match json.get("parent") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|i| {
                        i.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| err!("invalid 'parent' path in lock file"))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => None,
        };

        // An entry that is only pinned by its NAR hash is accepted (Nix warns).
        if !locked_ref.input.is_locked(settings)?
            && locked_ref.input.is_relative().is_none()
            && locked_ref.input.nar_hash()?.is_none()
        {
            return Err(err!(
                "Lock file contains unlocked input '{}'. Use '--allow-dirty-locks' to accept this lock file.",
                crate::attrs::attrs_to_json_string(&locked_ref.input.attrs)
            ));
        }

        locked_ref.input.set_final();

        Ok(LockedNode {
            locked_ref,
            original_ref,
            is_flake,
            build_time,
            parent_input_attr_path,
        })
    }
}

/// `std::put_time(gmtime(t), "%Y-%m-%d")`.
pub fn format_date(unix: u64) -> String {
    let (y, m, d, _, _, _) = civil_from_unix(unix);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `std::put_time(gmtime(t), "%F %T")`, as `nix flake metadata` shows
/// the inputs' `lastModified`.
pub fn format_date_time(unix: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_unix(unix);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// `std::put_time(gmtime(t), "%Y%m%d%H%M%S")`, as `lastModifiedDate`.
pub fn format_last_modified_date(unix: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_unix(unix);
    format!("{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}")
}

/// `gmtime`: the UTC date and time of a Unix time, by Howard Hinnant's
/// civil-from-days algorithm. This is brix's one calendar conversion (the
/// evaluator gets `lastModifiedDate` from its host). Date crates such as
/// `jiff` stop at the year 9999, while Nix prints whatever `gmtime` gives:
/// a `lastModified` of 253402300800 is `lastModifiedDate`
/// "100000101000000".
fn civil_from_unix(unix: u64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = unix as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// nlohmann's `dump(2)` formatting: two-space indent, `": "` separators,
/// one element per line, empty containers as `{}`/`[]`.
pub fn pretty_json(value: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut out,
        serde_json::ser::PrettyFormatter::with_indent(b"  "),
    );
    serde::Serialize::serialize(value, &mut ser).expect("JSON serialises");
    String::from_utf8(out).expect("JSON is UTF-8")
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = r#"{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1765186076,
        "narHash": "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=",
        "owner": "NixOS",
        "repo": "nixpkgs",
        "rev": "addf7cf5f383a3101ecfba091b98d0a1263dc9b8",
        "type": "github"
      },
      "original": {
        "owner": "nixos",
        "ref": "nixos-26.05",
        "repo": "nixpkgs",
        "type": "github"
      }
    },
    "root": {
      "inputs": {
        "nixpkgs": "nixpkgs",
        "other": [
          "nixpkgs"
        ]
      }
    }
  },
  "root": "root",
  "version": 7
}"#;

    #[test]
    fn parse_and_print_round_trip() {
        let lock = LockFile::parse(&FetchSettings::default(), SIMPLE, "flake.lock").unwrap();
        assert_eq!(lock.nodes.len(), 2);
        let root = lock.node(lock.root);
        assert_eq!(root.inputs["other"], Edge::Follows(vec!["nixpkgs".into()]));
        let Edge::Node(np) = root.inputs["nixpkgs"] else {
            panic!()
        };
        let locked = lock.node(np).locked.as_ref().unwrap();
        assert!(locked.is_flake);
        assert!(locked.locked_ref.input.is_final());
        assert_eq!(lock.to_json_string(), SIMPLE);
        assert!(lock
            .is_unlocked(&FetchSettings::default())
            .unwrap()
            .is_none());
        assert!(lock.check().is_ok());
        assert_eq!(lock.find_input(&["other".into()]).unwrap(), Some(np));
        assert_eq!(lock.find_input(&["nope".into()]).unwrap(), None);
    }

    #[test]
    fn duplicate_keys_get_suffixes() {
        let settings = FetchSettings::default();
        let mut lock = LockFile::new();
        let mk = |rev: char| {
            let attrs = crate::attrs::attrs([
                ("type", "github".into()),
                ("owner", "o".into()),
                ("repo", "r".into()),
                ("rev", rev.to_string().repeat(40).into()),
            ]);
            let r = FlakeRef::from_attrs(&settings, &attrs).unwrap();
            LockedNode {
                locked_ref: r.clone(),
                original_ref: r,
                is_flake: true,
                build_time: false,
                parent_input_attr_path: None,
            }
        };
        let a = lock.add_node(Node {
            inputs: BTreeMap::new(),
            locked: Some(mk('a')),
        });
        let b = lock.add_node(Node {
            inputs: BTreeMap::new(),
            locked: Some(mk('b')),
        });
        lock.node_mut(a).inputs.insert("dep".into(), Edge::Node(b));
        lock.node_mut(lock.root)
            .inputs
            .insert("dep".into(), Edge::Node(a));
        let (json, keys) = lock.to_json();
        assert_eq!(keys[&a], "dep");
        assert_eq!(keys[&b], "dep_2");
        assert_eq!(json["nodes"]["dep"]["inputs"]["dep"], "dep_2");
        let text = lock.to_json_string();
        let back = LockFile::parse(&settings, &text, "x").unwrap();
        assert_eq!(back.to_json_string(), text);
    }

    #[test]
    fn rejects_unlocked_and_bad_versions() {
        let settings = FetchSettings::default();
        let unlocked = SIMPLE.replace(
            "        \"rev\": \"addf7cf5f383a3101ecfba091b98d0a1263dc9b8\",\n",
            "",
        );
        // Only a narHash: accepted (Nix warns).
        assert!(LockFile::parse(&settings, &unlocked, "x").is_ok());
        let no_hash = unlocked.replace(
            "        \"narHash\": \"sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=\",\n",
            "",
        );
        let err = LockFile::parse(&settings, &no_hash, "x")
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Lock file contains unlocked input"),
            "{err}"
        );
        let v4 = SIMPLE.replace("\"version\": 7", "\"version\": 4");
        assert_eq!(
            LockFile::parse(&settings, &v4, "flake.lock")
                .unwrap_err()
                .to_string(),
            "lock file 'flake.lock' has unsupported version 4"
        );
    }

    #[test]
    fn follow_cycles_and_missing_follows() {
        let settings = FetchSettings::default();
        let text = SIMPLE.replace(
            "\"other\": [\n          \"nixpkgs\"\n        ]",
            "\"other\": [\n          \"other\"\n        ]",
        );
        let lock = LockFile::parse(&settings, &text, "x").unwrap();
        let err = lock.find_input(&["other".into()]).unwrap_err().to_string();
        assert_eq!(err, "follow cycle detected: [other -> other]");
        let text = SIMPLE.replace(
            "\"other\": [\n          \"nixpkgs\"\n        ]",
            "\"other\": [\n          \"missing\"\n        ]",
        );
        let lock = LockFile::parse(&settings, &text, "x").unwrap();
        assert_eq!(
            lock.check().unwrap_err().to_string(),
            "input 'other' follows a non-existent input 'missing'"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(1765186076), "2025-12-08");
        assert_eq!(format_last_modified_date(1765186076), "20251208092756");
        assert_eq!(format_date_time(1765186076), "2025-12-08 09:27:56");
        assert_eq!(format_date(0), "1970-01-01");
        // `lastModifiedDate` of a `path` input with these `lastModified`
        // values in `nix eval`.
        for (t, date) in [
            (253402300799, "99991231235959"),
            (253402300800, "100000101000000"),
            (300000000000, "114760815052000"),
        ] {
            assert_eq!(format_last_modified_date(t), date, "{t}");
        }
    }

    /// The conversion agrees with `jiff` wherever `jiff` has dates.
    #[cfg(feature = "native")]
    #[test]
    fn dates_agree_with_jiff() {
        let mut t: u64 = 0;
        while t < 253402300800 {
            let ts = jiff::Timestamp::from_second(t as i64).unwrap();
            assert_eq!(
                format_last_modified_date(t),
                ts.strftime("%Y%m%d%H%M%S").to_string()
            );
            assert_eq!(format_date(t), ts.strftime("%Y-%m-%d").to_string());
            assert_eq!(format_date_time(t), ts.strftime("%F %T").to_string());
            t = t * 3 / 2 + 86_399;
        }
    }

    /// `LockFile::operator==` compares the JSON: the numbering of the
    /// nodes and nodes no input reaches do not matter.
    #[test]
    fn lock_files_are_equal_when_their_json_is() {
        let lock = LockFile::parse(&FetchSettings::default(), SIMPLE, "flake.lock").unwrap();
        let Edge::Node(np) = lock.node(lock.root).inputs["nixpkgs"] else {
            panic!()
        };
        let mut renumbered = LockFile::new();
        // An unreachable node first, so that nixpkgs gets another id.
        renumbered.add_node(lock.node(np).clone());
        let np2 = renumbered.add_node(lock.node(np).clone());
        renumbered.node_mut(renumbered.root).inputs = lock.node(lock.root).inputs.clone();
        renumbered
            .node_mut(renumbered.root)
            .inputs
            .insert("nixpkgs".into(), Edge::Node(np2));
        assert_ne!(renumbered.nodes, lock.nodes);
        assert_eq!(renumbered, lock);
        renumbered
            .node_mut(renumbered.root)
            .inputs
            .insert("extra".into(), Edge::Follows(vec!["nixpkgs".into()]));
        assert_ne!(renumbered, lock);
    }

    #[test]
    fn diff_lists_changes() {
        let settings = FetchSettings::default();
        let old = LockFile::parse(&settings, SIMPLE, "x").unwrap();
        let new = LockFile::new();
        let d = LockFile::diff(&old, &new);
        assert_eq!(d, "• Removed input 'nixpkgs'\n• Removed input 'other'\n");
        let d = LockFile::diff(&new, &old);
        assert!(d.starts_with(
            "• Added input 'nixpkgs':\n    'github:NixOS/nixpkgs/addf7cf' (2025-12-08)\n"
        ));
    }
}
