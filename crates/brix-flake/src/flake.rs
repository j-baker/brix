//! Flakes and locking (`src/libflake/flake.cc`).
//!
//! `flake.nix` is Nix code, so this crate cannot read it by itself: the
//! facade evaluates the file and hands the result over as a
//! [`FlakeValue`] tree, which [`parse_flake_metadata`] turns into
//! [`FlakeMetadata`] with exactly Nix's rules. The lock algorithm
//! ([`lock_flake`]) is `lockFlake`/`computeLocks` ported over a
//! [`LockDeps`] trait that supplies fetching and `flake.nix` reading, and
//! is generic over the tree handle type so it does not care how trees are
//! represented.

use std::collections::{BTreeMap, BTreeSet};

use brix_store::{CanonPath, CanonPathError};

use crate::attrs::{Attr, Attrs};
use crate::flakeref::{parse_flake_ref, FlakeRef, ParseOptions};
use crate::input::{FetchSettings, Input};
use crate::lockfile::{
    parse_input_attr_path, print_input_attr_path, Edge, InputAttrPath, LockFile, LockedNode, Node,
    NodeId,
};
use crate::registry::UseRegistries;
use crate::{err, Error, Result};

/// `FlakeInput`: one entry of `inputs` in `flake.nix`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FlakeInput {
    pub ref_: Option<FlakeRef>,
    pub is_flake: bool,
    pub build_time: bool,
    /// Absolute (from the lock root) `follows` path.
    pub follows: Option<InputAttrPath>,
    /// `inputs.foo.inputs.bar = …` overrides for the input's own inputs.
    pub overrides: FlakeInputs,
}

impl FlakeInput {
    pub fn new(ref_: FlakeRef) -> FlakeInput {
        FlakeInput {
            ref_: Some(ref_),
            is_flake: true,
            ..Default::default()
        }
    }
}

/// `FlakeInputs`.
pub type FlakeInputs = BTreeMap<String, FlakeInput>;

/// What `readFlake` extracts from `flake.nix`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FlakeMetadata {
    pub description: Option<String>,
    pub inputs: FlakeInputs,
    /// `inputs.self.<attr>` (only `submodules` and `lfs` are allowed).
    pub self_attrs: Attrs,
}

/// A trivially evaluated `flake.nix` value, as the facade hands it over.
/// Only the shapes `readFlake` looks at are represented.
#[derive(Clone, Debug, PartialEq)]
pub enum FlakeValue {
    String(String),
    /// A path value, given relative to the flake directory as
    /// `flakeDir.path.makeRelative(path)` gives it: `.` for the directory
    /// itself, with `..` for paths outside it.
    Path(String),
    /// A path value of another source tree than the flake's (an absolute
    /// path literal), which no input may name: the path and the flake
    /// directory, as Nix shows them.
    ForeignPath {
        path: String,
        flake_dir: String,
    },
    Int(i64),
    Bool(bool),
    /// An attribute set, each attribute with its position.
    Attrs(BTreeMap<String, FlakeAttr>),
    List(Vec<FlakeValue>),
    /// A function; only `outputs` may be one. The formal parameter names
    /// (if the function has a `{ … }` pattern) are recorded because they
    /// implicitly declare inputs.
    Function {
        formals: Option<Vec<String>>,
    },
    /// Anything else (a derivation, null, …), with `showType`'s name.
    Other(String),
}

/// An attribute of a [`FlakeValue::Attrs`].
#[derive(Clone, Debug, PartialEq)]
pub struct FlakeAttr {
    pub value: FlakeValue,
    /// Where the attribute is defined, for the messages and traces that
    /// name it.
    pub pos: FlakePos,
}

/// A position in `flake.nix` (Nix's `PosIdx`): where an attribute is
/// defined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakePos {
    /// The position as Nix prints it in a message (`/path/flake.nix:3:5`,
    /// `«none»:0` for none).
    pub shown: String,
    /// The evaluating host's own handle for the position, opaque to this
    /// crate (the facade's is brix-eval's `SrcPos`): a trace that names
    /// the position ([`Error::trace_at`]) is shown with the code there by
    /// the host, which resolves it.
    pub handle: u32,
}

impl std::fmt::Display for FlakePos {
    /// As Nix prints a position in a message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown)
    }
}

impl FlakeValue {
    /// `showType`.
    fn type_name(&self) -> &str {
        match self {
            FlakeValue::String(_) => "a string",
            FlakeValue::Path(_) | FlakeValue::ForeignPath { .. } => "a path",
            FlakeValue::Int(_) => "an integer",
            FlakeValue::Bool(_) => "a Boolean",
            FlakeValue::Attrs(_) => "a set",
            FlakeValue::List(_) => "a list",
            FlakeValue::Function { .. } => "a function",
            FlakeValue::Other(t) => t,
        }
    }
}

/// Where a flake's directory lives: a tree (opaque to this module) plus
/// the subdirectory inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeDir<T> {
    pub tree: T,
    /// Relative path inside the tree, `""` for the root. For a relative
    /// `path:` input that climbs out of the tree it starts with `..`: Nix
    /// resolves those against where the tree is mounted (see
    /// [`resolve_relative`]).
    pub subdir: String,
}

impl<T> FlakeDir<T> {
    /// The path of `flake.lock` inside the tree.
    pub fn lock_file_path(&self) -> String {
        join_rel(&self.subdir, "flake.lock")
    }
}

/// Join two relative paths, treating `""` as the root.
pub fn join_rel(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// The directory a relative `path:` input names, given the directory
/// (inside the same tree) of the flake it is relative to:
/// `CanonPath(relativePath, parent)`.
///
/// Nix resolves against the absolute directory the tree is mounted at,
/// so a path that climbs above the tree's root names something outside
/// it, which pure evaluation may not read. Such a directory is kept
/// relative to the tree, starting with `..`: whoever reads it resolves
/// it against the tree's location, as Nix does.
pub fn resolve_relative(dir: &str, relative: &str) -> Result<String> {
    // A NUL byte is CanonPath's error.
    CanonPath::new(relative).map_err(|e: CanonPathError| err!("{}", e))?;
    let mut parts: Vec<&str> = dir
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    for component in relative.split('/') {
        match component {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            component => parts.push(component),
        }
    }
    Ok(parts.join("/"))
}

/// `Flake`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flake<T> {
    pub original_ref: FlakeRef,
    pub resolved_ref: FlakeRef,
    pub locked_ref: FlakeRef,
    pub dir: FlakeDir<T>,
    pub metadata: FlakeMetadata,
    /// The lock file would not be written (dirty working tree).
    pub force_dirty: bool,
}

/// `expectType`'s error: `value` is not of the `wanted` type.
fn expected(wanted: &str, value: &FlakeValue, pos: &FlakePos) -> Error {
    err!(
        "expected {} but got {} at {}",
        wanted,
        value.type_name(),
        pos
    )
}

/// `parseFlakeInputAttr`: a string, Boolean or integer input attribute.
fn parse_flake_input_attr(
    settings: &FetchSettings,
    name: &str,
    value: &FlakeValue,
    attrs: &mut Attrs,
) -> Result<()> {
    match value {
        FlakeValue::String(s) => {
            attrs.insert(name.into(), Attr::String(s.clone()));
        }
        FlakeValue::Bool(b) => {
            attrs.insert(name.into(), Attr::Bool(*b));
        }
        FlakeValue::Int(n) => {
            if *n < 0 {
                return Err(err!(
                    "negative value given for flake input attribute {}: {}",
                    name,
                    n
                ));
            }
            attrs.insert(name.into(), Attr::Int(*n as u64));
        }
        // Nix takes a `publicKeys` of any other type as its JSON, with the
        // `verified-fetches` feature: the evaluating host makes that a
        // string, so another type gets here only without the feature.
        other if name == "publicKeys" => {
            settings.require_feature("verified-fetches")?;
            return Err(err!(
                "flake input attribute 'publicKeys' is {} that was not given as JSON",
                other.type_name()
            ));
        }
        other => {
            return Err(err!(
                "flake input attribute '{}' is {} while a string, Boolean, or integer is expected",
                name,
                other.type_name()
            ))
        }
    }
    Ok(())
}

/// `parseFlakeInput`.
fn parse_flake_input(
    settings: &FetchSettings,
    value: &FlakeValue,
    pos: &FlakePos,
    lock_root_attr_path: &[String],
) -> Result<FlakeInput> {
    let FlakeValue::Attrs(fields) = value else {
        return Err(expected("a set", value, pos));
    };
    let mut input = FlakeInput {
        is_flake: true,
        ..Default::default()
    };
    let mut attrs = Attrs::new();
    let mut url: Option<String> = None;

    for (
        name,
        FlakeAttr {
            value: v,
            pos: attr_pos,
        },
    ) in fields
    {
        let res = (|| -> Result<()> {
            match name.as_str() {
                "url" => {
                    let u = match v {
                        FlakeValue::String(s) => s.clone(),
                        FlakeValue::Path(rel) => format!("path:{rel}"),
                        FlakeValue::ForeignPath { path, flake_dir } => {
                            return Err(err!(
                            "input attribute path '{}' at {} must be in the same source tree as {}",
                            path,
                            attr_pos,
                            flake_dir
                        ))
                        }
                        other => return Err(expected("a string or a path", other, attr_pos)),
                    };
                    attrs.insert("url".into(), Attr::String(u.clone()));
                    url = Some(u);
                }
                "flake" => match v {
                    FlakeValue::Bool(b) => input.is_flake = *b,
                    other => return Err(expected("a Boolean", other, attr_pos)),
                },
                "buildTime" => match v {
                    FlakeValue::Bool(b) => {
                        input.build_time = *b;
                        if *b {
                            settings.require_feature("build-time-fetch-tree")?;
                        }
                    }
                    other => return Err(expected("a Boolean", other, attr_pos)),
                },
                "inputs" => {
                    input.overrides =
                        parse_flake_inputs(settings, v, attr_pos, lock_root_attr_path, false)?.0;
                }
                "follows" => match v {
                    FlakeValue::String(s) => {
                        let mut follows = lock_root_attr_path.to_vec();
                        follows.extend(parse_input_attr_path(s)?);
                        input.follows = Some(follows);
                    }
                    other => return Err(expected("a string", other, attr_pos)),
                },
                _ => parse_flake_input_attr(settings, name, v, &mut attrs)?,
            }
            Ok(())
        })();
        res.map_err(|e| {
            e.trace_at(
                attr_pos,
                format!("while evaluating flake attribute '{name}'"),
            )
        })?;
    }

    if attrs.contains_key("type") {
        input.ref_ = Some(
            FlakeRef::from_attrs(settings, &attrs)
                .map_err(|e| e.trace_at(pos, "while evaluating flake input"))?,
        );
    } else {
        attrs.remove("url");
        if let Some((name, _)) = attrs.iter().next() {
            return Err(err!(
                "unexpected flake input attribute '{}', at {}",
                name,
                pos
            ));
        }
        if let Some(url) = url {
            input.ref_ = Some(parse_flake_ref(
                settings,
                &url,
                &ParseOptions {
                    allow_missing: true,
                    is_flake: input.is_flake,
                    preserve_relative_paths: true,
                    base_dir: None,
                },
            )?);
        }
    }

    if input.ref_.is_some() && input.follows.is_some() {
        return Err(err!(
            "flake input has both a flake reference and a follows attribute, at {}",
            pos
        ));
    }
    Ok(input)
}

/// `parseFlakeInputs`: the `inputs` attribute set. Returns the inputs and
/// the attributes of `inputs.self` (only allowed at the top level).
pub fn parse_flake_inputs(
    settings: &FetchSettings,
    value: &FlakeValue,
    pos: &FlakePos,
    lock_root_attr_path: &[String],
    allow_self: bool,
) -> Result<(FlakeInputs, Attrs)> {
    let FlakeValue::Attrs(fields) = value else {
        return Err(expected("a set", value, pos));
    };
    let mut inputs = FlakeInputs::new();
    let mut self_attrs = Attrs::new();
    for (
        name,
        FlakeAttr {
            value: v,
            pos: input_pos,
        },
    ) in fields
    {
        if name == "self" {
            if !allow_self {
                return Err(err!("'self' input attribute not allowed at {}", input_pos));
            }
            let FlakeValue::Attrs(self_fields) = v else {
                return Err(expected("a set", v, input_pos));
            };
            for (k, sv) in self_fields {
                parse_flake_input_attr(settings, k, &sv.value, &mut self_attrs)?;
            }
        } else {
            inputs.insert(
                name.clone(),
                parse_flake_input(settings, v, input_pos, lock_root_attr_path)?,
            );
        }
    }
    Ok((inputs, self_attrs))
}

/// `readFlake`'s interpretation of the top-level attribute set of
/// `flake.nix`. `flake_ref_for_errors` is the resolved reference, used in
/// messages.
pub fn parse_flake_metadata(
    settings: &FetchSettings,
    top: &FlakeValue,
    lock_root_attr_path: &[String],
    flake_ref_for_errors: &str,
) -> Result<FlakeMetadata> {
    let FlakeValue::Attrs(fields) = top else {
        return Err(err!(
            "flake '{}' is {} while a set was expected",
            flake_ref_for_errors,
            top.type_name()
        ));
    };
    let mut metadata = FlakeMetadata::default();

    if let Some(desc) = fields.get("description") {
        match &desc.value {
            FlakeValue::String(s) => metadata.description = Some(s.clone()),
            other => return Err(expected("a string", other, &desc.pos)),
        }
    }

    if let Some(inputs) = fields.get("inputs") {
        let (inputs, self_attrs) = parse_flake_inputs(
            settings,
            &inputs.value,
            &inputs.pos,
            lock_root_attr_path,
            true,
        )?;
        metadata.inputs = inputs;
        metadata.self_attrs = self_attrs;
    }

    match fields.get("outputs").map(|a| (&a.value, &a.pos)) {
        Some((FlakeValue::Function { formals }, _)) => {
            if let Some(formals) = formals {
                for formal in formals {
                    if formal != "self" {
                        // `inputs.emplace(name, FlakeInput{.ref = parseFlakeRef(name)})`:
                        // the reference is parsed before `emplace` looks for
                        // the name, so a formal that is not a flake reference
                        // is an error even when `inputs` declares it.
                        let ref_ = parse_flake_ref(settings, formal, &ParseOptions::flake())?;
                        metadata
                            .inputs
                            .entry(formal.clone())
                            .or_insert_with(|| FlakeInput::new(ref_));
                    }
                }
            }
        }
        Some((other, pos)) => return Err(expected("a function", other, pos)),
        None => {
            return Err(err!(
                "flake '{}' lacks attribute 'outputs'",
                flake_ref_for_errors
            ))
        }
    }

    if let Some(config) = fields.get("nixConfig") {
        let FlakeValue::Attrs(settings_map) = &config.value else {
            return Err(expected("a set", &config.value, &config.pos));
        };
        // brix applies no flake configuration (`--accept-flake-config`), so
        // the settings are only checked as `readFlake` checks them. (Nix
        // also copies a path setting to the store; the facade checks that
        // it could.)
        for (name, FlakeAttr { value: v, .. }) in settings_map {
            match v {
                FlakeValue::String(_)
                | FlakeValue::Path(_)
                | FlakeValue::ForeignPath { .. }
                | FlakeValue::Int(_)
                | FlakeValue::Bool(_) => {}
                FlakeValue::List(items) => {
                    for item in items {
                        match item {
                            FlakeValue::String(_) => {}
                            _ => {
                                return Err(err!(
                                    "list element in flake configuration setting '{}' is {} while a string is expected",
                                    name,
                                    v.type_name()
                                ))
                            }
                        }
                    }
                }
                other => {
                    return Err(err!(
                        "flake configuration setting '{}' is {}",
                        name,
                        other.type_name()
                    ))
                }
            }
        }
    }

    for (name, attr) in fields {
        if !matches!(
            name.as_str(),
            "description" | "inputs" | "outputs" | "nixConfig"
        ) {
            return Err(err!(
                "flake '{}' has an unsupported attribute '{}', at {}",
                flake_ref_for_errors,
                name,
                attr.pos
            ));
        }
    }

    Ok(metadata)
}

/// `applySelfAttrs`: `inputs.self.submodules`/`lfs` modify the flake's
/// own locked reference.
pub fn apply_self_attrs(locked_ref: &FlakeRef, metadata: &FlakeMetadata) -> Result<FlakeRef> {
    let mut new_ref = locked_ref.clone();
    for (name, value) in &metadata.self_attrs {
        if name != "submodules" && name != "lfs" {
            return Err(err!("flake 'self' attribute '{}' is not supported", name));
        }
        new_ref.input.attrs.insert(name.clone(), value.clone());
    }
    Ok(new_ref)
}

/// `LockFlags`.
#[derive(Clone, Debug)]
pub struct LockFlags {
    /// Ignore the existing lock file entirely.
    pub recreate_lock_file: bool,
    /// Whether the lock file may change at all (`--no-update-lock-file`
    /// makes a needed change an error).
    pub update_lock_file: bool,
    /// Whether a changed lock file would be written back. When not, a
    /// change only marks the flake dirty (`builtins.getFlake`, pure
    /// evaluation).
    pub write_lock_file: bool,
    /// Error if the result has an unlocked input.
    pub fail_on_unlocked: bool,
    /// Whether registries may be used to resolve indirect references.
    pub use_registries: bool,
    /// Whether unlocked inputs may be fetched at all (`false` in pure
    /// mode: "cannot update unlocked flake input").
    pub allow_unlocked: bool,
    /// `--override-input`.
    pub input_overrides: BTreeMap<InputAttrPath, FlakeRef>,
    /// `--update-input`.
    pub input_updates: BTreeSet<InputAttrPath>,
    /// `--reference-lock-file`: lock file contents to start from instead
    /// of the flake's own.
    pub reference_lock_file: Option<String>,
    /// `--output-lock-file`: where to write the new lock file instead of
    /// the flake's own `flake.lock`. It is written even when unchanged.
    pub output_lock_file: Option<String>,
}

impl Default for LockFlags {
    fn default() -> Self {
        LockFlags {
            recreate_lock_file: false,
            update_lock_file: true,
            write_lock_file: true,
            fail_on_unlocked: false,
            use_registries: true,
            allow_unlocked: true,
            input_overrides: BTreeMap::new(),
            input_updates: BTreeSet::new(),
            reference_lock_file: None,
            output_lock_file: None,
        }
    }
}

impl LockFlags {
    /// What pure evaluation uses: every input must already be locked.
    pub fn pure() -> LockFlags {
        LockFlags {
            update_lock_file: false,
            write_lock_file: false,
            fail_on_unlocked: false,
            use_registries: false,
            allow_unlocked: false,
            ..Default::default()
        }
    }
}

/// What the lock algorithm needs from the outside world.
pub trait LockDeps {
    /// A handle to a fetched source tree.
    type Tree: Clone;

    /// `getFlake`: fetch `flake_ref` (resolving through registries as
    /// allowed) and read its `flake.nix`.
    fn get_flake(
        &mut self,
        flake_ref: &FlakeRef,
        use_registries: UseRegistries,
        lock_root_attr_path: &[String],
    ) -> Result<Flake<Self::Tree>>;

    /// `readFlake` at a directory inside an already fetched tree (for
    /// relative `path:` inputs).
    fn read_flake_at(
        &mut self,
        flake_ref: &FlakeRef,
        dir: &FlakeDir<Self::Tree>,
        lock_root_attr_path: &[String],
    ) -> Result<Flake<Self::Tree>>;

    /// Fetch a non-flake input: the tree, the resolved and the locked
    /// input, and the registry's extra attrs.
    fn fetch_tree(
        &mut self,
        input: &Input,
        use_registries: UseRegistries,
    ) -> Result<FetchedInput<Self::Tree>>;

    /// The contents of a file in a tree, or `None` if it does not exist.
    fn read_file(&mut self, tree: &Self::Tree, path: &str) -> Result<Option<String>>;

    /// `Input::getSourcePath`: the directory on this machine that holds
    /// the input's files, where its lock file can be written (a `path:`
    /// input's directory, a local Git checkout's working tree); `None`
    /// for anything fetched from elsewhere.
    fn source_path(&mut self, input: &Input) -> Result<Option<String>>;

    /// How Nix shows the path of `flake.nix` in `dir` (`Flake::path`), for
    /// messages.
    fn show_flake_path(&mut self, dir: &FlakeDir<Self::Tree>) -> String;

    /// Nix's `warn`: the lock algorithm warns as it goes, so that a
    /// warning is seen even when locking fails later.
    fn warn(&mut self, msg: &str);

    /// The inputs the lock algorithm is about to fetch, one after the
    /// other, through [`LockDeps::get_flake`] and [`LockDeps::fetch_tree`]
    /// with these registry settings: an implementation may start fetching
    /// them all at once. A hint only: each is still asked for in turn, and
    /// anything that goes wrong here must be reported there.
    fn prefetch(&mut self, inputs: &[(Input, UseRegistries)]) {
        let _ = inputs;
    }
}

/// The result of fetching an input for locking.
#[derive(Clone, Debug)]
pub struct FetchedInput<T> {
    pub tree: T,
    pub resolved: Input,
    pub locked: Input,
    pub extra_attrs: Attrs,
}

/// What `lockFlake` decided to do with the new lock file. This crate does
/// not touch the file system: the caller writes the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockWrite {
    /// Nothing to write: the lock file is unchanged, may not be written
    /// (the flake is then dirty), or has an unlocked input. Nix has warned
    /// through [`LockDeps::warn`] where it does.
    Nothing,
    /// Write `contents` to the `--output-lock-file` path, silently.
    Output { path: String, contents: String },
    /// Write `contents` to the flake's own lock file at `path`, announced
    /// by [`LockWrite::announcement`] first.
    Source { path: String, contents: String },
}

impl LockWrite {
    /// The warning Nix prints before it writes the flake's own lock file,
    /// given whether the file `existed` and the [`LockedFlake::diff`].
    pub fn announcement(path: &str, existed: bool, diff: &str) -> String {
        // `chomp` and `PathFmt`.
        let diff = diff.trim_end_matches([' ', '\n', '\r', '\t']);
        if !existed {
            format!("creating lock file \"{path}\": \n{diff}")
        } else if diff.is_empty() {
            format!("updating lock file \"{path}\"")
        } else {
            format!("updating lock file \"{path}\":\n{diff}")
        }
    }
}

/// `LockedFlake`.
#[derive(Clone, Debug)]
pub struct LockedFlake<T> {
    pub flake: Flake<T>,
    pub lock_file: LockFile,
    /// Where each lock node's flake directory is; `callFlake` uses this to
    /// build the `overrides` argument of `call-flake.nix`.
    pub node_paths: BTreeMap<NodeId, FlakeDir<T>>,
    /// The lock file differs from the one the flake had.
    pub changed: bool,
    /// `LockFile::diff` of old against new.
    pub diff: String,
    /// What to do with the new lock file.
    pub write: LockWrite,
}

struct OverrideTarget<T> {
    input: FlakeInput,
    /// Relative `path:` references are resolved against this directory.
    source: FlakeDir<T>,
    /// The input path of the flake that declared the override; `None` for
    /// `--override-input`.
    parent_input_attr_path: Option<InputAttrPath>,
}

/// One node of `computeLocks`: the arguments that stay the same for all
/// of the node's inputs.
struct Level<'a, 'l, T> {
    /// The node whose inputs are locked.
    node: NodeId,
    /// Its path in the lock graph.
    prefix: &'a [String],
    /// The node of an old lock file whose entries can be copied.
    old: Option<(&'l LockFile, NodeId)>,
    /// What `follows` are relative to: the node's flake when it is newly
    /// locked, the lock root when its entry was copied.
    follows_prefix: &'a [String],
    /// The directory of the node's flake.
    source: &'a FlakeDir<T>,
    /// Whether the old lock's `follows` can be taken as they are, without
    /// checking that the flake still declares them.
    trust_lock: bool,
}

/// What `computeLocks` does with one input. The plans of all of a node's
/// inputs are made before any of them is processed, which is sound
/// because processing an input only adds overrides below its own path.
struct InputPlan<'l, T> {
    id: String,
    path: InputAttrPath,
    /// An ancestor or the command line overrides the declaration.
    overridden: bool,
    /// For a relative `path:` input: its directory, inside the tree of the
    /// flake it is relative to (the overrider's, if any). Such a flake is
    /// read in place instead of fetched (a path that climbs out of the tree
    /// is resolved against where the tree is mounted). An error (a NUL
    /// byte in the path) is reported when the directory is needed.
    in_tree: Option<Result<FlakeDir<T>>>,
    action: Action<'l>,
}

enum Action<'l> {
    /// `follows`, resolved once the lock file is complete.
    Follows(InputAttrPath),
    /// The entry of the old lock file is kept: its reference did not
    /// change and nothing overrides it. With `refetch`, the flake is still
    /// read to lock the inputs below it again (one of them is to be
    /// updated, or a `follows` in the lock is no longer declared).
    CopyOld {
        old_lf: &'l LockFile,
        old_id: NodeId,
        refetch: bool,
    },
    /// A new lock entry.
    New(Box<NewEntry>),
    /// An error, reported when the input's turn comes.
    Fail(Error),
}

/// What [`Action::New`] locks.
struct NewEntry {
    input: FlakeInput,
    input_ref: FlakeRef,
    /// What the entry records as `original`.
    original_ref: FlakeRef,
    /// `overriddenParentPath`.
    parent: Option<InputAttrPath>,
}

impl<T> InputPlan<'_, T> {
    /// The input this plan fetches, if any.
    fn fetches(&self) -> Option<&Input> {
        if self.in_tree.is_some() {
            return None;
        }
        match &self.action {
            Action::CopyOld {
                old_lf,
                old_id,
                refetch: true,
            } => Some(&locked_entry(old_lf, *old_id).locked_ref.input),
            Action::New(entry) => Some(&entry.input_ref.input),
            _ => None,
        }
    }
}

/// The lock entry of a node that is not the root.
fn locked_entry(lock: &LockFile, id: NodeId) -> &LockedNode {
    lock.node(id)
        .locked
        .as_ref()
        .expect("invariant: only the root node has no lock entry")
}

struct Locker<'a, D: LockDeps> {
    settings: &'a FetchSettings,
    deps: &'a mut D,
    flags: &'a LockFlags,
    use_registries_inputs: UseRegistries,
    overrides: BTreeMap<InputAttrPath, OverrideTarget<D::Tree>>,
    explicit_cli_overrides: BTreeSet<InputAttrPath>,
    overrides_used: BTreeSet<InputAttrPath>,
    updates_used: BTreeSet<InputAttrPath>,
    node_paths: BTreeMap<NodeId, FlakeDir<D::Tree>>,
    new_lock: LockFile,
    parents: Vec<FlakeRef>,
    /// How Nix shows the top-level flake's `flake.nix`, for the registry
    /// warning.
    flake_path: String,
}

/// `lockFlake(settings, state, topRef, lockFlags, flake)`: compute the
/// lock file for `flake`, reusing the entries of its existing lock file
/// where the inputs did not change, and decide what to do with it.
/// `topRef` is `flake.original_ref`.
pub fn lock_flake<D: LockDeps>(
    settings: &FetchSettings,
    deps: &mut D,
    flake: Flake<D::Tree>,
    flags: &LockFlags,
) -> Result<LockedFlake<D::Tree>> {
    let locked_ref_text = flake.locked_ref.to_string();
    lock_flake_inner(settings, deps, flake, flags).map_err(|e| {
        e.trace(format!(
            "while updating the lock file of flake '{locked_ref_text}'"
        ))
    })
}

fn lock_flake_inner<D: LockDeps>(
    settings: &FetchSettings,
    deps: &mut D,
    mut flake: Flake<D::Tree>,
    flags: &LockFlags,
) -> Result<LockedFlake<D::Tree>> {
    let use_registries_inputs = if flags.use_registries {
        UseRegistries::Limited
    } else {
        UseRegistries::No
    };

    let old_lock_file = match &flags.reference_lock_file {
        Some(_) if !settings.allow_dirty => {
            return Err(err!(
                "reference lock file was provided, but the `allow-dirty` setting is set to false"
            ))
        }
        Some(text) => LockFile::parse(settings, text, "reference lock file")?,
        None => read_lock_file(settings, deps, &flake.dir)?,
    };

    let flake_path = deps.show_flake_path(&flake.dir);
    let mut locker = Locker {
        settings,
        deps,
        flags,
        use_registries_inputs,
        overrides: BTreeMap::new(),
        explicit_cli_overrides: BTreeSet::new(),
        overrides_used: BTreeSet::new(),
        updates_used: BTreeSet::new(),
        node_paths: BTreeMap::new(),
        new_lock: LockFile::new(),
        parents: Vec::new(),
        flake_path,
    };

    for (path, ref_) in &flags.input_overrides {
        locker.overrides.insert(
            path.clone(),
            OverrideTarget {
                input: FlakeInput::new(ref_.clone()),
                // Relative overrides are interpreted relative to the
                // top-level flake.
                source: flake.dir.clone(),
                parent_input_attr_path: None,
            },
        );
        locker.explicit_cli_overrides.insert(path.clone());
    }

    let root = locker.new_lock.root;
    locker.node_paths.insert(root, flake.dir.clone());
    locker.compute_locks(
        &flake.metadata.inputs,
        &Level {
            node: root,
            prefix: &[],
            old: (!flags.recreate_lock_file).then_some((&old_lock_file, old_lock_file.root)),
            follows_prefix: &[],
            source: &flake.dir,
            trust_lock: false,
        },
    )?;

    for (path, ref_) in &flags.input_overrides {
        if !locker.overrides_used.contains(path) {
            locker.deps.warn(&format!(
                "the flag '--override-input {} {}' does not match any input",
                print_input_attr_path(path),
                ref_
            ));
        }
    }
    for path in &flags.input_updates {
        if !locker.updates_used.contains(path) {
            locker.deps.warn(&format!(
                "'{}' does not match any input of this flake",
                print_input_attr_path(path)
            ));
        }
    }

    // Check 'follows' inputs.
    locker.new_lock.check()?;

    let new_lock = locker.new_lock;
    let node_paths = locker.node_paths;
    // `LockFile::operator==` compares the JSON.
    let changed = new_lock != old_lock_file;
    let diff = LockFile::diff(&old_lock_file, &new_lock);
    let write = if changed || flags.output_lock_file.is_some() {
        lock_file_write(settings, deps, &mut flake, flags, &new_lock, &diff)?
    } else {
        LockWrite::Nothing
    };

    Ok(LockedFlake {
        flake,
        lock_file: new_lock,
        node_paths,
        changed,
        diff,
        write,
    })
}

/// The end of `lockFlake`: whether the new lock file can and may be
/// written, and where.
///
/// Nix writes the file here and then reads the flake again, since writing
/// may have changed its source. brix leaves the writing to the caller and
/// returns the flake as it was locked.
fn lock_file_write<D: LockDeps>(
    settings: &FetchSettings,
    deps: &mut D,
    flake: &mut Flake<D::Tree>,
    flags: &LockFlags,
    new_lock: &LockFile,
    diff: &str,
) -> Result<LockWrite> {
    let top_ref = &flake.original_ref;
    if !flags.write_lock_file {
        deps.warn(&format!(
            "not writing modified lock file of flake '{top_ref}':\n{}",
            diff.trim_end_matches([' ', '\n', '\r', '\t'])
        ));
        flake.force_dirty = true;
        return Ok(LockWrite::Nothing);
    }
    let source_path = deps.source_path(&top_ref.input)?;
    if source_path.is_none() && flags.output_lock_file.is_none() {
        return Err(err!(
            "cannot write modified lock file of flake '{}' (use '--no-write-lock-file' to ignore)",
            top_ref
        ));
    }
    if let Some(unlocked) = new_lock.is_unlocked(settings)? {
        if flags.fail_on_unlocked {
            return Err(err!(
                "Not writing lock file of flake '{}' because it has an unlocked input ('{}'). Use '--allow-dirty-locks' to allow this anyway.",
                top_ref,
                unlocked
            ));
        }
        if settings.warn_dirty {
            deps.warn(&format!(
                "not writing lock file of flake '{top_ref}' because it has an unlocked input ('{unlocked}')"
            ));
        }
        return Ok(LockWrite::Nothing);
    }
    if !flags.update_lock_file {
        return Err(err!(
            "flake '{}' requires lock file changes but they're not allowed due to '--no-update-lock-file'",
            top_ref
        ));
    }
    let contents = format!("{}\n", new_lock.to_json_string());
    Ok(match (&flags.output_lock_file, source_path) {
        (Some(path), _) => LockWrite::Output {
            path: path.clone(),
            contents,
        },
        (None, Some(dir)) => LockWrite::Source {
            path: format!("{dir}/{}", join_rel(&top_ref.subdir, "flake.lock")),
            contents,
        },
        (None, None) => unreachable!("checked above"),
    })
}

/// `readLockFile`: the flake's `flake.lock`, or an empty lock file.
fn read_lock_file<D: LockDeps>(
    settings: &FetchSettings,
    deps: &mut D,
    dir: &FlakeDir<D::Tree>,
) -> Result<LockFile> {
    match deps.read_file(&dir.tree, &dir.lock_file_path())? {
        Some(text) => LockFile::parse(settings, &text, &dir.lock_file_path()),
        None => Ok(LockFile::new()),
    }
}

/// `printLiteralString`: `s` as a Nix string literal.
fn nix_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl<D: LockDeps> Locker<'_, D> {
    /// `computeLocks`: lock the inputs of `level.node`.
    fn compute_locks(
        &mut self,
        flake_inputs: &FlakeInputs,
        level: &Level<'_, '_, D::Tree>,
    ) -> Result<()> {
        // Overrides declared by this flake (`inputs.a.inputs.b = …`).
        for (id, input) in flake_inputs {
            let mut path = level.prefix.to_vec();
            path.push(id.clone());
            self.add_overrides(input, &path, level.source, level.prefix);
        }

        // Overrides of inputs this flake does not have.
        let missing: Vec<String> = self
            .overrides
            .keys()
            .filter_map(|path| {
                let (id, parent) = path.split_last().expect("override paths are not empty");
                (parent == level.prefix && !flake_inputs.contains_key(id)).then(|| {
                    format!(
                        "input '{}' has an override for a non-existent input '{}'",
                        print_input_attr_path(level.prefix),
                        id
                    )
                })
            })
            .collect();
        for msg in missing {
            self.deps.warn(&msg);
        }

        let plans: Vec<InputPlan<'_, D::Tree>> = flake_inputs
            .iter()
            .map(|(id, input2)| self.plan_input(level, id, input2))
            .collect();

        // Every fetch the loop below will make, announced before the first
        // so that they can run at once.
        let fetches: Vec<(Input, UseRegistries)> = plans
            .iter()
            .filter_map(InputPlan::fetches)
            .map(|input| (input.clone(), self.use_registries_inputs))
            .collect();
        if !fetches.is_empty() {
            self.deps.prefetch(&fetches);
        }

        for plan in plans {
            let path_s = print_input_attr_path(&plan.path);
            self.compute_input(level, plan)
                .map_err(|e| e.trace(format!("while updating the flake input '{path_s}'")))?;
        }
        Ok(())
    }

    fn add_overrides(
        &mut self,
        input: &FlakeInput,
        prefix: &[String],
        source: &FlakeDir<D::Tree>,
        input_attr_path_prefix: &[String],
    ) {
        for (id_override, input_override) in &input.overrides {
            let mut path = prefix.to_vec();
            path.push(id_override.clone());
            // `overrides.emplace`: the first override of a path wins. The
            // command line's are added first, then each flake's from the
            // top down, so the outermost declaration takes precedence.
            if input_override.ref_.is_some() || input_override.follows.is_some() {
                self.overrides
                    .entry(path.clone())
                    .or_insert_with(|| OverrideTarget {
                        input: input_override.clone(),
                        source: source.clone(),
                        parent_input_attr_path: Some(input_attr_path_prefix.to_vec()),
                    });
            }
            self.add_overrides(input_override, &path, source, input_attr_path_prefix);
        }
    }

    /// The reference an input declaration names: its `url` (or attrs), or
    /// for a bare `inputs.<id> = { … }` the indirect `flake:<id>`.
    fn input_ref(&self, id: &str, input: &FlakeInput) -> Result<FlakeRef> {
        match &input.ref_ {
            Some(r) => Ok(r.clone()),
            None => FlakeRef::from_attrs(
                self.settings,
                &crate::attrs::attrs([("type", "indirect".into()), ("id", id.into())]),
            ),
        }
    }

    /// The input's entry in the existing lock file, unless it is scheduled
    /// for update.
    fn old_lock<'l>(
        &self,
        id: &str,
        input_attr_path: &[String],
        old_node: Option<(&'l LockFile, NodeId)>,
    ) -> Option<(&'l LockFile, NodeId)> {
        match old_node {
            Some((old_lf, old_id)) if !self.flags.input_updates.contains(input_attr_path) => {
                match old_lf.node(old_id).inputs.get(id) {
                    Some(Edge::Node(child)) => Some((old_lf, *child)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether a flake whose lock entry is copied must be fetched anyway:
    /// an input below it is scheduled for update, or (when the lock is not
    /// trusted) a `follows` in the lock is no longer declared.
    fn must_refetch(
        &self,
        input_attr_path: &[String],
        old_lf: &LockFile,
        old_id: NodeId,
        trust_lock: bool,
    ) -> bool {
        let update_below = self
            .flags
            .input_updates
            .range(input_attr_path.to_vec()..)
            .next()
            .is_some_and(|p| p.len() > input_attr_path.len() && p.starts_with(input_attr_path));
        update_below
            || (!trust_lock
                && old_lf.node(old_id).inputs.iter().any(|(name, edge)| {
                    let mut override_path = input_attr_path.to_vec();
                    override_path.push(name.clone());
                    matches!(edge, Edge::Follows(_)) && !self.overrides.contains_key(&override_path)
                }))
    }

    /// The first half of `computeLocks`' loop body: what to do with the
    /// input `id` of `level.node`, without doing it.
    fn plan_input<'l>(
        &self,
        level: &Level<'_, 'l, D::Tree>,
        id: &str,
        input2: &FlakeInput,
    ) -> InputPlan<'l, D::Tree> {
        let mut path = level.prefix.to_vec();
        path.push(id.to_owned());
        // Do we have an override for this input from one of the ancestors?
        let overrider = self.overrides.get(&path);
        let input = match overrider {
            // The "flakeness" of the input is kept even when overridden.
            Some(o) => FlakeInput {
                is_flake: input2.is_flake,
                ..o.input.clone()
            },
            None => input2.clone(),
        };
        let plan = |in_tree, action| InputPlan {
            id: id.to_owned(),
            path: path.clone(),
            overridden: overrider.is_some(),
            in_tree,
            action,
        };

        // `follows` is resolved later (it may refer to an input not yet
        // processed).
        if let Some(target) = &input.follows {
            return plan(None, Action::Follows(target.clone()));
        }
        let input_ref = match self.input_ref(id, &input) {
            Ok(r) => r,
            Err(e) => return plan(None, Action::Fail(e)),
        };

        // Relative `path:` inputs are resolved relative to the overrider,
        // and remember the input path of the flake they are relative to
        // (`overriddenParentPath`; none for a command-line override).
        let (in_tree, parent) = match input_ref.input.is_relative() {
            Some(rel) => {
                let base = overrider.map_or(level.source, |o| &o.source);
                let dir = resolve_relative(&base.subdir, &rel).map(|subdir| FlakeDir {
                    tree: base.tree.clone(),
                    subdir,
                });
                let parent = match overrider {
                    Some(o) => o.parent_input_attr_path.clone(),
                    None => Some(level.prefix.to_vec()),
                };
                (Some(dir), parent)
            }
            None => (None, None),
        };

        // Copy the entry of the old lock file if its reference did not
        // change and neither a higher-level flake nor the command line
        // overrides it.
        let has_cli_override = self.explicit_cli_overrides.contains(&path);
        if let Some((old_lf, old_id)) = self.old_lock(id, &path, level.old) {
            if let Some(ol) = &old_lf.node(old_id).locked {
                if ol.original_ref.canonicalize(&*self.settings.warn)
                    == input_ref.canonicalize(&*self.settings.warn)
                    && ol.parent_input_attr_path == parent
                    && !has_cli_override
                {
                    let refetch = self.must_refetch(&path, old_lf, old_id, level.trust_lock);
                    return plan(
                        in_tree,
                        Action::CopyOld {
                            old_lf,
                            old_id,
                            refetch,
                        },
                    );
                }
            }
        }

        if !self.flags.allow_unlocked {
            let locked = match input_ref.input.is_locked(self.settings) {
                Ok(locked) => locked,
                Err(e) => return plan(in_tree, Action::Fail(e)),
            };
            if !locked && in_tree.is_none() {
                let msg = format!(
                    "cannot update unlocked flake input '{}' in pure mode",
                    print_input_attr_path(&path)
                );
                return plan(in_tree, Action::Fail(err!("{}", msg)));
            }
        }

        // With a command-line override the *original* reference is
        // recorded, so that the override does not stick.
        let original_ref = match (&input2.ref_, has_cli_override) {
            (Some(r), true) => r.clone(),
            _ => input_ref.clone(),
        };
        plan(
            in_tree,
            Action::New(Box::new(NewEntry {
                input,
                input_ref,
                original_ref,
                parent,
            })),
        )
    }

    /// The second half of `computeLocks`' loop body: carry out `plan`.
    fn compute_input(
        &mut self,
        level: &Level<'_, '_, D::Tree>,
        plan: InputPlan<'_, D::Tree>,
    ) -> Result<()> {
        let InputPlan {
            id,
            path,
            overridden,
            in_tree,
            action,
        } = plan;
        if overridden {
            self.overrides_used.insert(path.clone());
        }
        if let Action::Follows(target) = action {
            self.new_lock
                .node_mut(level.node)
                .inputs
                .insert(id, Edge::Follows(target));
            return Ok(());
        }
        self.updates_used.insert(path.clone());

        match action {
            Action::Follows(_) => unreachable!("handled above"),
            Action::Fail(e) => Err(e),
            Action::CopyOld {
                old_lf,
                old_id,
                refetch,
            } => {
                let ol = locked_entry(old_lf, old_id);
                let child = self.new_lock.add_node(Node {
                    inputs: BTreeMap::new(),
                    locked: Some(ol.clone()),
                });
                self.new_lock
                    .node_mut(level.node)
                    .inputs
                    .insert(id, Edge::Node(child));
                if refetch {
                    let in_tree = in_tree.transpose()?;
                    let input_flake = self.get_input_flake(&ol.locked_ref, in_tree, &path)?;
                    self.node_paths.insert(child, input_flake.dir.clone());
                    self.compute_locks(
                        &input_flake.metadata.inputs,
                        &Level {
                            node: child,
                            prefix: &path,
                            old: Some((old_lf, old_id)),
                            follows_prefix: level.follows_prefix,
                            source: &input_flake.dir,
                            trust_lock: false,
                        },
                    )
                } else {
                    // No need to fetch this flake; but there may be new
                    // overrides on its inputs, so synthesise them from the
                    // lock.
                    let fake_inputs = old_lf
                        .node(old_id)
                        .inputs
                        .iter()
                        .map(|(name, edge)| {
                            let fake = match edge {
                                Edge::Node(child) => {
                                    let locked = locked_entry(old_lf, *child);
                                    FlakeInput {
                                        ref_: Some(locked.original_ref.clone()),
                                        is_flake: locked.is_flake,
                                        ..Default::default()
                                    }
                                }
                                Edge::Follows(follows) => {
                                    let mut absolute = level.follows_prefix.to_vec();
                                    absolute.extend(follows.iter().cloned());
                                    FlakeInput {
                                        follows: Some(absolute),
                                        ..Default::default()
                                    }
                                }
                            };
                            (name.clone(), fake)
                        })
                        .collect();
                    self.compute_locks(
                        &fake_inputs,
                        &Level {
                            node: child,
                            prefix: &path,
                            old: Some((old_lf, old_id)),
                            follows_prefix: level.follows_prefix,
                            source: level.source,
                            trust_lock: true,
                        },
                    )
                }
            }
            Action::New(entry) if entry.input.is_flake => {
                let NewEntry {
                    input,
                    input_ref,
                    original_ref,
                    parent,
                } = *entry;
                let input_flake = self.get_input_flake(&input_ref, in_tree.transpose()?, &path)?;
                // A fetched input is final: its attrs are exactly what was
                // fetched.
                let mut locked_ref = input_flake.locked_ref.clone();
                locked_ref.input.set_final();
                let child = self.new_lock.add_node(Node {
                    inputs: BTreeMap::new(),
                    locked: Some(LockedNode {
                        locked_ref,
                        original_ref,
                        is_flake: true,
                        build_time: input.build_time,
                        parent_input_attr_path: parent,
                    }),
                });
                self.new_lock
                    .node_mut(level.node)
                    .inputs
                    .insert(id, Edge::Node(child));

                // Guard against circular flake imports.
                if self.parents.contains(&input_ref) {
                    return Err(err!("found circular import of flake '{}'", input_ref));
                }
                self.parents.push(input_ref.clone());
                // Recursively process the inputs of this flake, using its
                // own lock file.
                self.node_paths.insert(child, input_flake.dir.clone());
                let res = read_lock_file(self.settings, self.deps, &input_flake.dir).and_then(
                    |child_lock| {
                        self.compute_locks(
                            &input_flake.metadata.inputs,
                            &Level {
                                node: child,
                                prefix: &path,
                                old: Some((&child_lock, child_lock.root)),
                                follows_prefix: &path,
                                source: &input_flake.dir,
                                trust_lock: false,
                            },
                        )
                    },
                );
                self.parents.pop();
                res?;
                self.warn_registry(&path, &input_ref, &input_flake.resolved_ref);
                Ok(())
            }
            Action::New(entry) => {
                // A non-flake input: just a tree.
                let NewEntry {
                    input,
                    input_ref,
                    original_ref,
                    parent,
                } = *entry;
                let (dir, locked_ref) = match in_tree.transpose()? {
                    Some(dir) => (dir, input_ref.clone()),
                    None => {
                        let fetched = self
                            .deps
                            .fetch_tree(&input_ref.input, self.use_registries_inputs)?;
                        let subdir = input_ref.subdir.clone();
                        let resolved_ref = FlakeRef::new(fetched.resolved, subdir.clone());
                        self.warn_registry(&path, &input_ref, &resolved_ref);
                        let mut locked_ref = FlakeRef::new(fetched.locked, subdir.clone());
                        locked_ref.input.set_final();
                        let dir = FlakeDir {
                            tree: fetched.tree,
                            subdir,
                        };
                        (dir, locked_ref)
                    }
                };
                let child = self.new_lock.add_node(Node {
                    inputs: BTreeMap::new(),
                    locked: Some(LockedNode {
                        locked_ref,
                        original_ref,
                        is_flake: false,
                        build_time: input.build_time,
                        parent_input_attr_path: parent,
                    }),
                });
                self.node_paths.insert(child, dir);
                self.new_lock
                    .node_mut(level.node)
                    .inputs
                    .insert(id, Edge::Node(child));
                Ok(())
            }
        }
    }

    /// `getInputFlake`: a relative `path:` input is read from inside the
    /// parent's tree; anything else is fetched.
    fn get_input_flake(
        &mut self,
        flake_ref: &FlakeRef,
        in_tree: Option<FlakeDir<D::Tree>>,
        input_attr_path: &[String],
    ) -> Result<Flake<D::Tree>> {
        match in_tree {
            Some(dir) => self.deps.read_flake_at(flake_ref, &dir, input_attr_path),
            // Nix's `getInputFlake` ignores the registry setting it is
            // given (`UseRegistries::All` for a command-line override) and
            // always passes `useRegistriesInputs` (flake.cc).
            None => self
                .deps
                .get_flake(flake_ref, self.use_registries_inputs, input_attr_path),
        }
    }

    /// `warnRegistry`: Determinate Nix deprecates registry references in
    /// flake inputs, and warns about the top-level flake's own.
    fn warn_registry(&mut self, path: &[String], input_ref: &FlakeRef, resolved_ref: &FlakeRef) {
        if path.len() == 1 && !input_ref.input.is_direct() {
            let msg = format!(
                "Flake input '{0}' uses the flake registry. Using the registry in flake inputs is deprecated in Determinate Nix. To make your flake future-proof, add the following to '{1}':\n\n  inputs.{0}.url = {2};\n\nFor more information, see: https://github.com/DeterminateSystems/nix-src/issues/37",
                print_input_attr_path(path),
                self.flake_path,
                nix_string_literal(&resolved_ref.to_string()),
            );
            self.deps.warn(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;

    fn s(v: &str) -> FlakeValue {
        FlakeValue::String(v.into())
    }

    /// An attribute set whose attributes are all at `«test»:<name>`.
    fn set(pairs: Vec<(&str, FlakeValue)>) -> FlakeValue {
        FlakeValue::Attrs(
            pairs
                .into_iter()
                .map(|(k, value)| (k.to_string(), FlakeAttr { value, pos: pos(k) }))
                .collect(),
        )
    }

    /// The position [`set`] gives the attribute `name`.
    fn pos(name: &str) -> FlakePos {
        FlakePos {
            shown: format!("«test»:{name}"),
            handle: 0,
        }
    }

    #[test]
    fn parses_inputs_declaration() {
        let settings = FetchSettings::default();
        let top = set(vec![
            ("description", s("hi")),
            (
                "inputs",
                set(vec![
                    (
                        "nixpkgs",
                        set(vec![("url", s("github:NixOS/nixpkgs/nixos-25.05"))]),
                    ),
                    (
                        "other",
                        set(vec![
                            ("url", s("git+https://x/y")),
                            ("flake", FlakeValue::Bool(false)),
                            (
                                "inputs",
                                set(vec![("nixpkgs", set(vec![("follows", s("nixpkgs"))]))]),
                            ),
                        ]),
                    ),
                    ("local", set(vec![("url", FlakeValue::Path("sub".into()))])),
                    (
                        "typed",
                        set(vec![
                            ("type", s("github")),
                            ("owner", s("a")),
                            ("repo", s("b")),
                            ("lastModified", FlakeValue::Int(5)),
                        ]),
                    ),
                    ("self", set(vec![("submodules", FlakeValue::Bool(true))])),
                ]),
            ),
            (
                "outputs",
                FlakeValue::Function {
                    formals: Some(vec!["self".into(), "nixpkgs".into(), "implicit".into()]),
                },
            ),
            (
                "nixConfig",
                set(vec![(
                    "extra-substituters",
                    FlakeValue::List(vec![s("https://c")]),
                )]),
            ),
        ]);
        let md = parse_flake_metadata(&settings, &top, &[], "flake:x").unwrap();
        assert_eq!(md.description.as_deref(), Some("hi"));
        assert_eq!(
            md.inputs["nixpkgs"].ref_.as_ref().unwrap().to_string(),
            "github:NixOS/nixpkgs/nixos-25.05"
        );
        let other = &md.inputs["other"];
        assert!(!other.is_flake);
        assert_eq!(other.ref_.as_ref().unwrap().to_string(), "git+https://x/y");
        assert_eq!(
            other.overrides["nixpkgs"].follows,
            Some(vec!["nixpkgs".to_string()])
        );
        assert_eq!(
            md.inputs["local"].ref_.as_ref().unwrap().to_string(),
            "path:sub"
        );
        assert_eq!(
            md.inputs["typed"].ref_.as_ref().unwrap().to_attrs(),
            attrs([
                ("type", "github".into()),
                ("owner", "a".into()),
                ("repo", "b".into()),
                ("lastModified", 5u64.into()),
            ])
        );
        assert_eq!(
            md.inputs["implicit"].ref_.as_ref().unwrap().to_string(),
            "flake:implicit"
        );
        assert_eq!(md.self_attrs, attrs([("submodules", true.into())]));
    }

    /// `nixConfig` is checked as `readFlake` checks it.
    #[test]
    fn rejects_bad_flake_configuration() {
        let settings = FetchSettings::default();
        let with_config = |config: FlakeValue| {
            set(vec![
                ("outputs", FlakeValue::Function { formals: None }),
                ("nixConfig", set(vec![("x", config)])),
            ])
        };
        let ok = with_config(FlakeValue::List(vec![s("a")]));
        assert!(parse_flake_metadata(&settings, &ok, &[], "flake:x").is_ok());
        let bad_list = with_config(FlakeValue::List(vec![FlakeValue::Int(1)]));
        assert_eq!(
            parse_flake_metadata(&settings, &bad_list, &[], "flake:x")
                .unwrap_err()
                .to_string(),
            "list element in flake configuration setting 'x' is a list while a string is expected"
        );
        let bad = with_config(FlakeValue::Other("null".into()));
        assert_eq!(
            parse_flake_metadata(&settings, &bad, &[], "flake:x")
                .unwrap_err()
                .to_string(),
            "flake configuration setting 'x' is null"
        );
    }

    #[test]
    fn rejects_bad_declarations() {
        let settings = FetchSettings::default();
        let no_outputs = set(vec![("inputs", set(vec![]))]);
        assert_eq!(
            parse_flake_metadata(&settings, &no_outputs, &[], "flake:x")
                .unwrap_err()
                .to_string(),
            "flake 'flake:x' lacks attribute 'outputs'"
        );
        let extra = set(vec![
            ("outputs", FlakeValue::Function { formals: None }),
            ("bogus", s("x")),
        ]);
        assert_eq!(
            parse_flake_metadata(&settings, &extra, &[], "flake:x")
                .unwrap_err()
                .to_string(),
            "flake 'flake:x' has an unsupported attribute 'bogus', at «test»:bogus"
        );
        let both = set(vec![
            ("outputs", FlakeValue::Function { formals: None }),
            (
                "inputs",
                set(vec![(
                    "a",
                    set(vec![("url", s("github:a/b")), ("follows", s("c"))]),
                )]),
            ),
        ]);
        let err = parse_flake_metadata(&settings, &both, &[], "flake:x").unwrap_err();
        assert_eq!(
            err.message(),
            "flake input has both a flake reference and a follows attribute, at «test»:a"
        );
        let nested_self = set(vec![
            ("outputs", FlakeValue::Function { formals: None }),
            (
                "inputs",
                set(vec![(
                    "a",
                    set(vec![("inputs", set(vec![("self", set(vec![]))]))]),
                )]),
            ),
        ]);
        let err = parse_flake_metadata(&settings, &nested_self, &[], "flake:x").unwrap_err();
        assert!(err
            .to_string()
            .contains("'self' input attribute not allowed at «test»:self"));
        // `expectType`: the value's type and the attribute's position, as
        // `nix flake metadata` reports them for such a `flake.nix`.
        let typed = |top: FlakeValue| {
            parse_flake_metadata(&settings, &top, &[], "flake:x")
                .unwrap_err()
                .to_string()
        };
        let outputs = || ("outputs", FlakeValue::Function { formals: None });
        assert_eq!(
            typed(set(vec![outputs(), ("description", FlakeValue::Int(1))])),
            "expected a string but got an integer at «test»:description"
        );
        assert_eq!(
            typed(set(vec![("outputs", FlakeValue::Int(1))])),
            "expected a function but got an integer at «test»:outputs"
        );
        assert_eq!(
            typed(set(vec![outputs(), ("nixConfig", FlakeValue::Int(1))])),
            "expected a set but got an integer at «test»:nixConfig"
        );
        assert_eq!(
            typed(set(vec![
                outputs(),
                ("inputs", set(vec![("a", FlakeValue::Int(1))]))
            ])),
            "expected a set but got an integer at «test»:a"
        );
        assert_eq!(
            typed(set(vec![
                outputs(),
                (
                    "inputs",
                    set(vec![("a", set(vec![("url", FlakeValue::Int(1))]))])
                )
            ])),
            "expected a string or a path but got an integer at «test»:url"
        );
        assert_eq!(
            typed(set(vec![
                outputs(),
                ("inputs", set(vec![("a", set(vec![("flake", s("x"))]))]))
            ])),
            "expected a Boolean but got a string at «test»:flake"
        );
    }

    /// `parseFlakeInput`'s traces name the attribute, or the input, they
    /// are about (`addTrace(state.positions[…], …)`), a nested input's
    /// outermost first; its other errors have none.
    #[test]
    fn input_errors_are_traced_at_their_attributes() {
        let settings = FetchSettings::default();
        let traces = |inputs: FlakeValue| -> Vec<(Option<String>, String)> {
            let top = set(vec![
                ("outputs", FlakeValue::Function { formals: None }),
                ("inputs", inputs),
            ]);
            parse_flake_metadata(&settings, &top, &[], "flake:x")
                .unwrap_err()
                .traces_at()
                .into_iter()
                .map(|(pos, trace)| (pos.map(|p| p.shown.clone()), trace.to_owned()))
                .collect()
        };
        let traced = |pos: &str, trace: &str| (Some(format!("«test»:{pos}")), trace.to_owned());
        let nested = set(vec![(
            "a",
            set(vec![(
                "inputs",
                set(vec![("b", set(vec![("url", FlakeValue::Int(1))]))]),
            )]),
        )]);
        assert_eq!(
            traces(nested),
            [
                traced("inputs", "while evaluating flake attribute 'inputs'"),
                traced("url", "while evaluating flake attribute 'url'"),
            ]
        );
        let untyped = set(vec![("a", set(vec![("type", s("path"))]))]);
        assert_eq!(
            traces(untyped),
            [traced("a", "while evaluating flake input")]
        );
        let unexpected = set(vec![("a", set(vec![("foo", s("x"))]))]);
        assert!(traces(unexpected).is_empty());
    }

    #[test]
    fn relative_paths() {
        assert_eq!(resolve_relative("", "./foo").unwrap(), "foo");
        assert_eq!(resolve_relative("a/b", "../c").unwrap(), "a/c");
        assert_eq!(resolve_relative("a/b", "./x//y/../z").unwrap(), "a/b/x/z");
        assert_eq!(resolve_relative("a", "..").unwrap(), "");
        // Out of the tree: what is above it is reached through its mount.
        assert_eq!(resolve_relative("a", "../../c").unwrap(), "../c");
        assert_eq!(resolve_relative("", "../x/../../y").unwrap(), "../../y");
        assert_eq!(resolve_relative("../c", "../d").unwrap(), "../d");
        assert_eq!(join_rel("", "flake.lock"), "flake.lock");
        assert_eq!(join_rel("sub", "flake.lock"), "sub/flake.lock");
    }

    /// A fake world of flakes for exercising the lock algorithm without
    /// fetching anything: trees are named by string.
    #[derive(Default)]
    struct World {
        /// flake id (as `flake:<id>` resolves) → (locked attrs, metadata, lock file text)
        flakes: BTreeMap<String, (Attrs, FlakeMetadata, Option<String>)>,
        fetches: Vec<String>,
        /// Announced by `prefetch` and not fetched yet, with the registry
        /// setting announced.
        announced: Vec<(String, UseRegistries)>,
        /// Each `prefetch` call.
        batches: Vec<Vec<String>>,
        /// What the registry resolves an indirect reference to (by its
        /// text), where a test needs a resolved reference that differs.
        registry: BTreeMap<String, String>,
        /// Trees whose files are on this machine: tree key → directory.
        sources: BTreeMap<String, String>,
        /// What [`LockDeps::warn`] was told.
        warnings: Vec<String>,
    }

    impl World {
        /// Every fetch must have been announced, with the same registry
        /// setting, before it happens.
        fn fetching(&mut self, key: &str, use_registries: UseRegistries) {
            let at = self
                .announced
                .iter()
                .position(|(k, _)| k == key)
                .unwrap_or_else(|| panic!("'{key}' fetched without being announced"));
            let (_, announced) = self.announced.remove(at);
            assert_eq!(announced, use_registries, "registries for '{key}'");
            self.fetches.push(key.to_owned());
        }
    }

    impl LockDeps for World {
        type Tree = String;

        fn get_flake(
            &mut self,
            flake_ref: &FlakeRef,
            use_registries: UseRegistries,
            _lock_root_attr_path: &[String],
        ) -> Result<Flake<String>> {
            let key = flake_ref.to_string();
            self.fetching(&key, use_registries);
            let (locked, metadata, _) = self
                .flakes
                .get(&key)
                .ok_or_else(|| err!("unknown flake '{}'", key))?
                .clone();
            let locked_ref = FlakeRef::from_attrs(&FetchSettings::default(), &locked)?;
            Ok(Flake {
                original_ref: flake_ref.clone(),
                resolved_ref: match self.registry.get(&key) {
                    Some(url) => {
                        parse_flake_ref(&FetchSettings::default(), url, &ParseOptions::flake())?
                    }
                    None => flake_ref.clone(),
                },
                locked_ref,
                dir: FlakeDir {
                    tree: key,
                    subdir: String::new(),
                },
                metadata,
                force_dirty: false,
            })
        }

        fn read_flake_at(
            &mut self,
            flake_ref: &FlakeRef,
            dir: &FlakeDir<String>,
            _lock_root_attr_path: &[String],
        ) -> Result<Flake<String>> {
            let key = format!("{}/{}", dir.tree, dir.subdir);
            let (_, metadata, _) = self
                .flakes
                .get(&key)
                .ok_or_else(|| err!("unknown flake dir '{}'", key))?
                .clone();
            Ok(Flake {
                original_ref: flake_ref.clone(),
                resolved_ref: flake_ref.clone(),
                locked_ref: flake_ref.clone(),
                dir: dir.clone(),
                metadata,
                force_dirty: false,
            })
        }

        fn fetch_tree(
            &mut self,
            input: &Input,
            use_registries: UseRegistries,
        ) -> Result<FetchedInput<String>> {
            let key = input.to_url_text(false)?;
            self.fetching(&key, use_registries);
            let (locked, _, _) = self
                .flakes
                .get(&key)
                .ok_or_else(|| err!("unknown tree '{}'", key))?
                .clone();
            Ok(FetchedInput {
                tree: key,
                resolved: input.clone(),
                locked: Input::from_attrs(&FetchSettings::default(), locked)?,
                extra_attrs: Attrs::new(),
            })
        }

        fn read_file(&mut self, tree: &String, path: &str) -> Result<Option<String>> {
            if path == "flake.lock" {
                return Ok(self.flakes.get(tree).and_then(|(_, _, lock)| lock.clone()));
            }
            Ok(None)
        }

        fn source_path(&mut self, input: &Input) -> Result<Option<String>> {
            Ok(self.sources.get(&input.to_url_text(false)?).cloned())
        }

        fn show_flake_path(&mut self, dir: &FlakeDir<String>) -> String {
            format!("«{}»/{}", dir.tree, join_rel(&dir.subdir, "flake.nix"))
        }

        fn warn(&mut self, msg: &str) {
            self.warnings.push(msg.to_owned());
        }

        fn prefetch(&mut self, inputs: &[(Input, UseRegistries)]) {
            let mut batch = Vec::new();
            for (input, use_registries) in inputs {
                let key = input.to_url_text(false).unwrap();
                batch.push(key.clone());
                self.announced.push((key, *use_registries));
            }
            self.batches.push(batch);
        }
    }

    fn locked_github(owner: &str, repo: &str, rev: char) -> Attrs {
        attrs([
            ("type", "github".into()),
            ("owner", owner.into()),
            ("repo", repo.into()),
            ("rev", rev.to_string().repeat(40).into()),
            (
                "narHash",
                "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into(),
            ),
            ("lastModified", 1u64.into()),
        ])
    }

    fn metadata(inputs: Vec<(&str, FlakeInput)>) -> FlakeMetadata {
        FlakeMetadata {
            inputs: inputs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ..Default::default()
        }
    }

    fn flake_input(url: &str) -> FlakeInput {
        FlakeInput::new(
            parse_flake_ref(&FetchSettings::default(), url, &ParseOptions::flake()).unwrap(),
        )
    }

    fn root_flake(inputs: Vec<(&str, FlakeInput)>, lock: Option<&str>) -> (World, Flake<String>) {
        let settings = FetchSettings::default();
        let mut world = World::default();
        world.flakes.insert(
            "root".into(),
            (attrs([]), metadata(vec![]), lock.map(str::to_string)),
        );
        // The root's files are on this machine, so its lock file can be
        // written.
        world
            .sources
            .insert("github:me/root".into(), "/src/root".into());
        let root_ref =
            parse_flake_ref(&settings, "github:me/root", &ParseOptions::flake()).unwrap();
        let flake = Flake {
            original_ref: root_ref.clone(),
            resolved_ref: root_ref.clone(),
            locked_ref: root_ref,
            dir: FlakeDir {
                tree: "root".into(),
                subdir: String::new(),
            },
            metadata: metadata(inputs),
            force_dirty: false,
        };
        (world, flake)
    }

    #[test]
    fn locks_fresh_inputs_and_follows() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![
                ("nixpkgs", flake_input("github:NixOS/nixpkgs/nixos-25.05")),
                ("lib", {
                    let mut i = flake_input("github:x/lib");
                    i.overrides.insert(
                        "nixpkgs".into(),
                        FlakeInput {
                            follows: Some(vec!["nixpkgs".into()]),
                            ..Default::default()
                        },
                    );
                    i
                }),
                ("data", {
                    let mut i = flake_input("github:x/data");
                    i.is_flake = false;
                    i
                }),
            ],
            None,
        );
        world.flakes.insert(
            "github:NixOS/nixpkgs/nixos-25.05".into(),
            (
                locked_github("NixOS", "nixpkgs", 'a'),
                metadata(vec![]),
                None,
            ),
        );
        world.flakes.insert(
            "github:x/lib".into(),
            (
                locked_github("x", "lib", 'b'),
                metadata(vec![("nixpkgs", flake_input("github:NixOS/nixpkgs"))]),
                None,
            ),
        );
        world.flakes.insert(
            "github:x/data".into(),
            (locked_github("x", "data", 'c'), metadata(vec![]), None),
        );

        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert!(locked.changed);
        let json = locked.lock_file.to_json_string();
        assert!(
            json.contains("\"nixpkgs\": [\n          \"nixpkgs\"\n        ]"),
            "{json}"
        );
        assert!(json.contains("\"flake\": false"), "{json}");
        assert_eq!(locked.lock_file.nodes.len(), 4);
        assert!(locked.lock_file.is_unlocked(&settings).unwrap().is_none());
        // The lib's own nixpkgs was never fetched: it follows ours.
        assert!(!world.fetches.contains(&"github:NixOS/nixpkgs".to_string()));
        assert_eq!(locked.node_paths.len(), 4);
        // The three inputs were announced together, before the first was
        // fetched, and nothing announced went unfetched; the lib's inputs
        // are all follows, so it announces nothing.
        assert_eq!(
            world.batches,
            [[
                "github:x/data",
                "github:x/lib",
                "github:NixOS/nixpkgs/nixos-25.05"
            ]]
        );
        assert!(world.announced.is_empty(), "{:?}", world.announced);

        // Locking again with the result as the existing lock changes
        // nothing and fetches nothing.
        let (mut world2, mut flake2) = root_flake(vec![], Some(&json));
        flake2.metadata = locked.flake.metadata.clone();
        world2.flakes.extend(
            world
                .flakes
                .iter()
                .filter(|(k, _)| *k != "root")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        let again = lock_flake(&settings, &mut world2, flake2, &LockFlags::pure()).unwrap();
        assert!(!again.changed, "{}", again.diff);
        assert_eq!(again.lock_file.to_json_string(), json);
        assert!(world2.fetches.is_empty(), "{:?}", world2.fetches);
    }

    #[test]
    fn pure_mode_refuses_unlocked_inputs() {
        let settings = FetchSettings::default();
        let (mut world, flake) =
            root_flake(vec![("nixpkgs", flake_input("github:NixOS/nixpkgs"))], None);
        let err = lock_flake(&settings, &mut world, flake, &LockFlags::pure()).unwrap_err();
        assert_eq!(
            err.message(),
            "cannot update unlocked flake input 'nixpkgs' in pure mode"
        );
    }

    #[test]
    fn changed_input_is_refetched_and_cli_override_sticks() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("dep", flake_input("github:x/dep/v2"))], None);
        world.flakes.insert(
            "github:x/dep/v2".into(),
            (locked_github("x", "dep", 'd'), metadata(vec![]), None),
        );
        world.flakes.insert(
            "github:x/dep/v3".into(),
            (locked_github("x", "dep", 'e'), metadata(vec![]), None),
        );
        let first =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        let lock_text = first.lock_file.to_json_string();
        assert!(lock_text.contains(&"d".repeat(40)));

        // --override-input dep github:x/dep/v3: locked to v3 but the
        // original ref stays the flake's.
        let mut flags = LockFlags::default();
        flags.input_overrides.insert(
            vec!["dep".into()],
            parse_flake_ref(&settings, "github:x/dep/v3", &ParseOptions::flake()).unwrap(),
        );
        let (mut world2, mut flake2) = root_flake(vec![], Some(&lock_text));
        flake2.metadata = flake.metadata.clone();
        world2.flakes.extend(
            world
                .flakes
                .iter()
                .filter(|(k, _)| *k != "root")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        let second = lock_flake(&settings, &mut world2, flake2, &flags).unwrap();
        let dep = second.lock_file.node(NodeId(1)).locked.as_ref().unwrap();
        assert_eq!(dep.original_ref.to_string(), "github:x/dep/v2");
        assert!(dep.locked_ref.to_string().contains(&"e".repeat(40)));
        assert!(
            second.diff.contains("Updated input 'dep'"),
            "{}",
            second.diff
        );
    }

    /// `--update-input lib/nixpkgs`: the lock entry of `lib` is copied, but
    /// `lib` must be fetched again to re-lock its own `nixpkgs`. Both
    /// fetches are announced first, each with the root's other inputs
    /// that need one (none here).
    #[test]
    fn updating_a_nested_input_refetches_its_parent() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![
                ("lib", flake_input("github:x/lib")),
                ("data", {
                    let mut i = flake_input("github:x/data");
                    i.is_flake = false;
                    i
                }),
            ],
            None,
        );
        world.flakes.insert(
            "github:x/lib".into(),
            (
                locked_github("x", "lib", 'b'),
                metadata(vec![("nixpkgs", flake_input("github:NixOS/nixpkgs"))]),
                None,
            ),
        );
        world.flakes.insert(
            "github:NixOS/nixpkgs".into(),
            (
                locked_github("NixOS", "nixpkgs", 'a'),
                metadata(vec![]),
                None,
            ),
        );
        world.flakes.insert(
            "github:x/data".into(),
            (locked_github("x", "data", 'c'), metadata(vec![]), None),
        );
        let first =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        // The root's inputs together, then the lib's when it is read.
        assert_eq!(
            world.batches,
            [
                vec!["github:x/data", "github:x/lib"],
                vec!["github:NixOS/nixpkgs"]
            ]
        );
        let lock_text = first.lock_file.to_json_string();

        let mut flags = LockFlags::default();
        flags
            .input_updates
            .insert(vec!["lib".into(), "nixpkgs".into()]);
        let (mut world2, mut flake2) = root_flake(vec![], Some(&lock_text));
        flake2.metadata = flake.metadata.clone();
        world2.flakes.extend(
            world
                .flakes
                .iter()
                .filter(|(k, _)| *k != "root")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        // The lib is fetched again by its locked reference.
        let lib_locked = FlakeRef::from_attrs(&settings, &locked_github("x", "lib", 'b'))
            .unwrap()
            .to_string();
        let lib = world2.flakes["github:x/lib"].clone();
        world2.flakes.insert(lib_locked.clone(), lib);
        let second = lock_flake(&settings, &mut world2, flake2, &flags).unwrap();
        assert!(!second.changed, "{}", second.diff);
        // `data` keeps its entry unfetched; the lib's locked ref is
        // fetched to read its inputs, then its nixpkgs to re-lock it.
        assert_eq!(
            world2.batches,
            [vec![lib_locked.as_str()], vec!["github:NixOS/nixpkgs"]]
        );
        assert_eq!(
            world2.fetches,
            [lib_locked.as_str(), "github:NixOS/nixpkgs"]
        );
        assert!(world2.announced.is_empty(), "{:?}", world2.announced);
    }

    #[test]
    fn relative_path_inputs_are_read_in_place() {
        let settings = FetchSettings::default();
        let mut sub = flake_input("path:./sub");
        sub.ref_ = Some(
            parse_flake_ref(
                &settings,
                "path:./sub",
                &ParseOptions {
                    preserve_relative_paths: true,
                    ..ParseOptions::flake()
                },
            )
            .unwrap(),
        );
        let (mut world, flake) = root_flake(vec![("sub", sub)], None);
        world
            .flakes
            .insert("root/sub".into(), (attrs([]), metadata(vec![]), None));
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::pure()).unwrap();
        let node = locked.lock_file.node(NodeId(1)).locked.as_ref().unwrap();
        assert_eq!(node.parent_input_attr_path, Some(vec![]));
        assert_eq!(node.locked_ref.to_string(), "path:./sub");
        assert!(locked.lock_file.is_unlocked(&settings).unwrap().is_none());
        assert!(locked.lock_file.to_json_string().contains("\"parent\": []"));
    }

    fn follows(path: &[&str]) -> FlakeInput {
        FlakeInput {
            follows: Some(path.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn with_override(mut input: FlakeInput, id: &str, o: FlakeInput) -> FlakeInput {
        input.overrides.insert(id.into(), o);
        input
    }

    /// The edge at `path`, walking locked nodes from the root.
    fn edge(lock: &LockFile, path: &[&str]) -> Edge {
        let mut node = lock.root;
        let (last, init) = path.split_last().unwrap();
        for id in init {
            match &lock.node(node).inputs[*id] {
                Edge::Node(child) => node = *child,
                other => panic!("'{id}' is {other:?}"),
            }
        }
        lock.node(node).inputs[*last].clone()
    }

    fn locked_url(lock: &LockFile, path: &[&str]) -> String {
        match edge(lock, path) {
            Edge::Node(id) => lock
                .node(id)
                .locked
                .as_ref()
                .unwrap()
                .locked_ref
                .to_string(),
            other => panic!("{path:?} is {other:?}"),
        }
    }

    /// `--override-input hm/nixpkgs …` wins over the flake's own
    /// `inputs.hm.inputs.nixpkgs.follows`: Nix adds the command line's
    /// overrides first and keeps the first override of a path.
    #[test]
    fn command_line_overrides_win_over_the_flakes() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![
                ("nixpkgs", flake_input("github:x/np2")),
                (
                    "hm",
                    with_override(flake_input("github:x/hm"), "nixpkgs", follows(&["nixpkgs"])),
                ),
            ],
            None,
        );
        world.flakes.insert(
            "github:x/hm".into(),
            (
                locked_github("x", "hm", 'f'),
                metadata(vec![("nixpkgs", flake_input("github:x/np1"))]),
                None,
            ),
        );
        for (name, rev) in [("np2", '2'), ("np3", '3')] {
            world.flakes.insert(
                format!("github:x/{name}"),
                (locked_github("x", name, rev), metadata(vec![]), None),
            );
        }
        let mut flags = LockFlags::default();
        flags.input_overrides.insert(
            vec!["hm".into(), "nixpkgs".into()],
            parse_flake_ref(&settings, "github:x/np3", &ParseOptions::flake()).unwrap(),
        );
        let locked = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert!(
            locked_url(&locked.lock_file, &["hm", "nixpkgs"]).contains(&"3".repeat(40)),
            "{}",
            locked.lock_file.to_json_string()
        );
    }

    /// An ancestor's override wins over a descendant's for the same input:
    /// the root's `inputs.a.inputs.b.inputs.nixpkgs.follows = "nixpkgs"`
    /// over `a`'s own `inputs.b.inputs.nixpkgs.follows = "nixpkgs"`.
    #[test]
    fn an_ancestors_override_wins_over_a_descendants() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![
                ("nixpkgs", flake_input("github:x/np2")),
                (
                    "a",
                    with_override(
                        flake_input("github:x/a"),
                        "b",
                        with_override(FlakeInput::default(), "nixpkgs", follows(&["nixpkgs"])),
                    ),
                ),
            ],
            None,
        );
        world.flakes.insert(
            "github:x/a".into(),
            (
                locked_github("x", "a", 'a'),
                metadata(vec![
                    ("nixpkgs", flake_input("github:x/npa")),
                    (
                        "b",
                        // `a`'s follows are absolute: `a` is at `["a"]`.
                        with_override(
                            flake_input("github:x/b"),
                            "nixpkgs",
                            follows(&["a", "nixpkgs"]),
                        ),
                    ),
                ]),
                None,
            ),
        );
        world.flakes.insert(
            "github:x/b".into(),
            (
                locked_github("x", "b", 'b'),
                metadata(vec![("nixpkgs", flake_input("github:x/np1"))]),
                None,
            ),
        );
        for (name, rev) in [("np2", '2'), ("npa", 'c')] {
            world.flakes.insert(
                format!("github:x/{name}"),
                (locked_github("x", name, rev), metadata(vec![]), None),
            );
        }
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert_eq!(
            edge(&locked.lock_file, &["a", "b", "nixpkgs"]),
            Edge::Follows(vec!["nixpkgs".into()])
        );
    }

    #[test]
    fn circular_imports_are_detected() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("a", flake_input("github:x/a"))], None);
        world.flakes.insert(
            "github:x/a".into(),
            (
                locked_github("x", "a", 'a'),
                metadata(vec![("a", flake_input("github:x/a"))]),
                None,
            ),
        );
        let err = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap_err();
        assert_eq!(err.message(), "found circular import of flake 'github:x/a'");
    }

    fn relative_input(path: &str) -> FlakeInput {
        FlakeInput::new(
            parse_flake_ref(
                &FetchSettings::default(),
                path,
                &ParseOptions {
                    preserve_relative_paths: true,
                    ..ParseOptions::flake()
                },
            )
            .unwrap(),
        )
    }

    fn add(world: &mut World, key: &str, locked: Attrs, inputs: Vec<(&str, FlakeInput)>) {
        world
            .flakes
            .insert(key.into(), (locked, metadata(inputs), None));
    }

    /// A world with a root flake whose `dep` input has its own `nixpkgs`,
    /// locked once; returns the world for a second lock and the lock text.
    fn locked_world(dep_inputs: Vec<(&str, FlakeInput)>) -> (World, Flake<String>, String) {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("dep", flake_input("github:x/dep"))], None);
        add(
            &mut world,
            "github:x/dep",
            locked_github("x", "dep", 'd'),
            dep_inputs,
        );
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        add(
            &mut world,
            "github:x/np2",
            locked_github("x", "np2", '2'),
            vec![],
        );
        let first =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        let text = first.lock_file.to_json_string();
        let (mut world2, mut flake2) = root_flake(vec![], Some(&text));
        flake2.metadata = flake.metadata;
        world2
            .flakes
            .extend(world.flakes.into_iter().filter(|(k, _)| k != "root"));
        (world2, flake2, text)
    }

    #[test]
    fn a_changed_lock_file_is_written_to_the_flakes_source() {
        let settings = FetchSettings::default();
        let (mut world, mut flake) = root_flake(vec![("np", flake_input("github:x/np"))], None);
        flake.original_ref.subdir = "sub".into();
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        world
            .sources
            .insert("github:me/root?dir=sub".into(), "/src/root".into());
        // The source is looked up by the input (without `dir`).
        world
            .sources
            .insert("github:me/root".into(), "/src/root".into());
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert_eq!(
            locked.write,
            LockWrite::Source {
                path: "/src/root/sub/flake.lock".into(),
                contents: format!("{}\n", locked.lock_file.to_json_string()),
            }
        );
        assert!(world.warnings.is_empty(), "{:?}", world.warnings);
    }

    #[test]
    fn an_unchanged_lock_file_is_written_only_to_an_output_path() {
        let settings = FetchSettings::default();
        let (mut world, flake, text) = locked_world(vec![]);
        let again =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        assert!(!again.changed);
        assert_eq!(again.write, LockWrite::Nothing);

        let flags = LockFlags {
            output_lock_file: Some("/out/flake.lock".into()),
            ..LockFlags::default()
        };
        let again = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert_eq!(
            again.write,
            LockWrite::Output {
                path: "/out/flake.lock".into(),
                contents: format!("{text}\n"),
            }
        );
    }

    #[test]
    fn a_lock_file_without_a_source_cannot_be_written() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("np", flake_input("github:x/np"))], None);
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        world.sources.clear();
        let err =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "cannot write modified lock file of flake 'github:me/root' (use '--no-write-lock-file' to ignore)"
        );
        // With an output path there is somewhere to write it.
        let flags = LockFlags {
            output_lock_file: Some("/out/flake.lock".into()),
            ..LockFlags::default()
        };
        world.announced.clear();
        let locked = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert!(matches!(locked.write, LockWrite::Output { .. }));
    }

    #[test]
    fn a_lock_file_that_may_not_be_written_makes_the_flake_dirty() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("np", flake_input("github:x/np"))], None);
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        let flags = LockFlags {
            write_lock_file: false,
            ..LockFlags::default()
        };
        let locked = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert!(locked.flake.force_dirty);
        assert_eq!(locked.write, LockWrite::Nothing);
        assert_eq!(
            world.warnings,
            ["not writing modified lock file of flake 'github:me/root':\n• Added input 'np':\n    'github:x/np/1111111' (1970-01-01)"]
        );
    }

    #[test]
    fn needed_changes_are_an_error_without_update_lock_file() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("np", flake_input("github:x/np"))], None);
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        let flags = LockFlags {
            update_lock_file: false,
            ..LockFlags::default()
        };
        let err = lock_flake(&settings, &mut world, flake, &flags).unwrap_err();
        assert_eq!(
            err.render(),
            "error:\n       … while updating the lock file of flake 'github:me/root'\n\n       error: flake 'github:me/root' requires lock file changes but they're not allowed due to '--no-update-lock-file'"
        );
    }

    #[test]
    fn a_lock_file_with_an_unlocked_input_is_not_written() {
        let mut settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("np", flake_input("github:x/np"))], None);
        // What a dirty Git working tree locks to: no revision.
        add(
            &mut world,
            "github:x/np",
            attrs([
                ("type", "github".into()),
                ("owner", "x".into()),
                ("repo", "np".into()),
            ]),
            vec![],
        );
        let locked =
            lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        assert_eq!(locked.write, LockWrite::Nothing);
        assert_eq!(
            world.warnings,
            ["not writing lock file of flake 'github:me/root' because it has an unlocked input ('github:x/np')"]
        );

        // `warn-dirty = false` silences it.
        settings.warn_dirty = false;
        world.warnings.clear();
        world.announced.clear();
        lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        assert!(world.warnings.is_empty(), "{:?}", world.warnings);

        // `flake lock` fails instead.
        let flags = LockFlags {
            fail_on_unlocked: true,
            ..LockFlags::default()
        };
        world.announced.clear();
        let err = lock_flake(&settings, &mut world, flake, &flags).unwrap_err();
        assert_eq!(
            err.message(),
            "Not writing lock file of flake 'github:me/root' because it has an unlocked input ('github:x/np'). Use '--allow-dirty-locks' to allow this anyway."
        );
    }

    #[test]
    fn unused_overrides_and_updates_are_warned_about() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![(
                "np",
                with_override(flake_input("github:x/np"), "gone", follows(&["np"])),
            )],
            None,
        );
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        let mut flags = LockFlags::default();
        flags.input_overrides.insert(
            vec!["zzz".into()],
            parse_flake_ref(&settings, "github:x/np", &ParseOptions::flake()).unwrap(),
        );
        flags.input_updates.insert(vec!["nosuch".into()]);
        lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert_eq!(
            world.warnings,
            [
                "input '' has an override for a non-existent input 'zzz'",
                "input 'np' has an override for a non-existent input 'gone'",
                "the flag '--override-input zzz github:x/np' does not match any input",
                "'nosuch' does not match any input of this flake",
            ]
        );
    }

    #[test]
    fn registry_references_in_top_level_inputs_are_warned_about() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![(
                "dep",
                with_override(flake_input("flake:dep"), "inner", follows(&["dep"])),
            )],
            None,
        );
        world
            .registry
            .insert("flake:dep".into(), "github:x/dep".into());
        add(
            &mut world,
            "flake:dep",
            locked_github("x", "dep", 'd'),
            vec![("inner", flake_input("flake:inner"))],
        );
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert!(locked.lock_file.is_unlocked(&settings).unwrap().is_none());
        // The nested `flake:inner` follows `dep` and is not fetched; only
        // the top-level input is warned about.
        assert_eq!(
            world.warnings,
            ["Flake input 'dep' uses the flake registry. Using the registry in flake inputs is deprecated in Determinate Nix. To make your flake future-proof, add the following to '«root»/flake.nix':\n\n  inputs.dep.url = \"github:x/dep\";\n\nFor more information, see: https://github.com/DeterminateSystems/nix-src/issues/37"]
        );
    }

    #[test]
    fn nix_string_literals() {
        assert_eq!(
            nix_string_literal("a\"b\\c\n${d}$e"),
            r#""a\"b\\c\n\${d}$e""#
        );
    }

    #[test]
    fn lock_file_announcements() {
        assert_eq!(
            LockWrite::announcement("/f/flake.lock", false, "• Added input 'a':\n    'x'\n"),
            "creating lock file \"/f/flake.lock\": \n• Added input 'a':\n    'x'"
        );
        assert_eq!(
            LockWrite::announcement("/f/flake.lock", true, "• Updated input 'a'\n"),
            "updating lock file \"/f/flake.lock\":\n• Updated input 'a'"
        );
        assert_eq!(
            LockWrite::announcement("/f/flake.lock", true, ""),
            "updating lock file \"/f/flake.lock\""
        );
    }

    /// `inputs.dep.inputs.np.url = …` replaces the reference `dep`
    /// declares for its own input.
    #[test]
    fn nested_url_overrides_replace_the_inner_declaration() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![(
                "dep",
                with_override(
                    flake_input("github:x/dep"),
                    "np",
                    flake_input("github:x/np2"),
                ),
            )],
            None,
        );
        add(
            &mut world,
            "github:x/dep",
            locked_github("x", "dep", 'd'),
            vec![("np", flake_input("github:x/np"))],
        );
        add(
            &mut world,
            "github:x/np2",
            locked_github("x", "np2", '2'),
            vec![],
        );
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert!(locked_url(&locked.lock_file, &["dep", "np"]).contains(&"2".repeat(40)));
        let Edge::Node(id) = edge(&locked.lock_file, &["dep", "np"]) else {
            panic!()
        };
        assert_eq!(
            locked_entry(&locked.lock_file, id).original_ref.to_string(),
            "github:x/np2"
        );
        assert!(!world.fetches.contains(&"github:x/np".to_string()));
    }

    /// A relative override is resolved against the flake that declares
    /// it, and the entry records that flake as its parent.
    #[test]
    fn relative_overrides_are_resolved_against_the_overrider() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![(
                "dep",
                with_override(
                    flake_input("github:x/dep"),
                    "sub",
                    relative_input("path:./mine"),
                ),
            )],
            None,
        );
        add(
            &mut world,
            "github:x/dep",
            locked_github("x", "dep", 'd'),
            vec![("sub", relative_input("path:./sub"))],
        );
        add(&mut world, "root/mine", attrs([]), vec![]);
        let locked = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        let Edge::Node(id) = edge(&locked.lock_file, &["dep", "sub"]) else {
            panic!()
        };
        let entry = locked_entry(&locked.lock_file, id);
        assert_eq!(entry.locked_ref.to_string(), "path:./mine");
        assert_eq!(entry.parent_input_attr_path, Some(vec![]));
        assert_eq!(
            locked.node_paths[&id],
            FlakeDir {
                tree: "root".into(),
                subdir: "mine".into()
            }
        );
    }

    /// A relative `--override-input` (possible through the API only: the
    /// command line makes paths absolute) records no parent.
    #[test]
    fn relative_command_line_overrides_record_no_parent() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(vec![("sub", relative_input("path:./sub"))], None);
        add(&mut world, "root/sub2", attrs([]), vec![]);
        let mut flags = LockFlags::default();
        flags.input_overrides.insert(
            vec!["sub".into()],
            relative_input("path:./sub2").ref_.unwrap(),
        );
        let locked = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        let Edge::Node(id) = edge(&locked.lock_file, &["sub"]) else {
            panic!()
        };
        let entry = locked_entry(&locked.lock_file, id);
        assert_eq!(entry.locked_ref.to_string(), "path:./sub2");
        assert_eq!(entry.parent_input_attr_path, None);
        assert!(!locked.lock_file.to_json_string().contains("\"parent\""));
    }

    /// The lock says `dep/np` follows the root's `np`, but the root no
    /// longer declares that: `dep`'s entry is kept, but `dep` is fetched
    /// again (by its locked reference) to lock its own `np`.
    #[test]
    fn a_follows_that_disappeared_refetches_the_flake() {
        let settings = FetchSettings::default();
        let (mut world, flake) = root_flake(
            vec![
                ("np", flake_input("github:x/np2")),
                (
                    "dep",
                    with_override(flake_input("github:x/dep"), "np", follows(&["np"])),
                ),
            ],
            None,
        );
        add(
            &mut world,
            "github:x/dep",
            locked_github("x", "dep", 'd'),
            vec![("np", flake_input("github:x/np"))],
        );
        add(
            &mut world,
            "github:x/np",
            locked_github("x", "np", '1'),
            vec![],
        );
        add(
            &mut world,
            "github:x/np2",
            locked_github("x", "np2", '2'),
            vec![],
        );
        let first = lock_flake(&settings, &mut world, flake, &LockFlags::default()).unwrap();
        assert_eq!(
            edge(&first.lock_file, &["dep", "np"]),
            Edge::Follows(vec!["np".into()])
        );

        let text = first.lock_file.to_json_string();
        let (mut world2, flake2) = root_flake(
            vec![
                ("np", flake_input("github:x/np2")),
                ("dep", flake_input("github:x/dep")),
            ],
            Some(&text),
        );
        let dep_locked = FlakeRef::from_attrs(&settings, &locked_github("x", "dep", 'd'))
            .unwrap()
            .to_string();
        world2
            .flakes
            .extend(world.flakes.into_iter().filter(|(k, _)| k != "root"));
        let dep = world2.flakes["github:x/dep"].clone();
        world2.flakes.insert(dep_locked.clone(), dep);
        let second = lock_flake(&settings, &mut world2, flake2, &LockFlags::default()).unwrap();
        assert_eq!(world2.fetches, [dep_locked.as_str(), "github:x/np"]);
        assert!(locked_url(&second.lock_file, &["dep", "np"]).contains(&"1".repeat(40)));
        assert!(second.changed);
    }

    /// `--reference-lock-file`: its entries are reused as if they were the
    /// flake's own.
    #[test]
    fn a_reference_lock_file_replaces_the_flakes() {
        let mut settings = FetchSettings::default();
        let (mut world, flake, text) = locked_world(vec![("np", flake_input("github:x/np"))]);
        // The flake's own lock file is gone.
        world.flakes.get_mut("root").unwrap().2 = None;
        let flags = LockFlags {
            reference_lock_file: Some(text.clone()),
            ..LockFlags::default()
        };
        let locked = lock_flake(&settings, &mut world, flake.clone(), &flags).unwrap();
        assert!(world.fetches.is_empty(), "{:?}", world.fetches);
        assert!(!locked.changed);
        assert_eq!(locked.lock_file.to_json_string(), text);

        settings.allow_dirty = false;
        let err = lock_flake(&settings, &mut world, flake, &flags).unwrap_err();
        assert_eq!(
            err.message(),
            "reference lock file was provided, but the `allow-dirty` setting is set to false"
        );
    }

    /// `recreate_lock_file` ignores the existing lock: everything is
    /// fetched again by its original reference.
    #[test]
    fn recreating_the_lock_file_fetches_everything_again() {
        let settings = FetchSettings::default();
        let (mut world, flake, _) = locked_world(vec![("np", flake_input("github:x/np"))]);
        // Upstream moved on.
        world.flakes.get_mut("github:x/np").unwrap().0 = locked_github("x", "np", '3');
        let kept = lock_flake(&settings, &mut world, flake.clone(), &LockFlags::default()).unwrap();
        assert!(!kept.changed);
        assert!(world.fetches.is_empty());
        let flags = LockFlags {
            recreate_lock_file: true,
            ..LockFlags::default()
        };
        let recreated = lock_flake(&settings, &mut world, flake, &flags).unwrap();
        assert_eq!(world.fetches, ["github:x/dep", "github:x/np"]);
        assert!(locked_url(&recreated.lock_file, &["dep", "np"]).contains(&"3".repeat(40)));
        assert!(
            recreated.diff.contains("Updated input 'dep/np'"),
            "{}",
            recreated.diff
        );
    }

    #[test]
    fn build_time_inputs_need_their_experimental_feature() {
        let mut settings = FetchSettings::default();
        let top = set(vec![
            (
                "inputs",
                set(vec![(
                    "a",
                    set(vec![
                        ("url", s("github:a/b")),
                        ("buildTime", FlakeValue::Bool(true)),
                    ]),
                )]),
            ),
            ("outputs", FlakeValue::Function { formals: None }),
        ]);
        let err = parse_flake_metadata(&settings, &top, &[], "flake:x").unwrap_err();
        assert_eq!(
            err.message(),
            "experimental Nix feature 'build-time-fetch-tree' is disabled; add '--extra-experimental-features build-time-fetch-tree' to enable it"
        );
        settings
            .experimental_features
            .insert("build-time-fetch-tree".into());
        let md = parse_flake_metadata(&settings, &top, &[], "flake:x").unwrap();
        assert!(md.inputs["a"].build_time);
    }

    /// Nix parses a formal of `outputs` as a flake reference before it
    /// checks whether `inputs` declares it.
    #[test]
    fn every_formal_of_outputs_must_be_a_flake_reference() {
        let settings = FetchSettings::default();
        let top = set(vec![
            (
                "inputs",
                set(vec![("_foo", set(vec![("url", s("github:a/b"))]))]),
            ),
            (
                "outputs",
                FlakeValue::Function {
                    formals: Some(vec!["self".into(), "_foo".into()]),
                },
            ),
        ]);
        let err = parse_flake_metadata(&settings, &top, &[], "flake:x").unwrap_err();
        assert_eq!(
            err.to_string(),
            "flake reference '_foo' is not an absolute path"
        );
    }
}
