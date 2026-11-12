//! Installables: the things `nix build`, `nix eval`, `nix run` and friends
//! operate on. `flakeref#attr.path^outputs`, a store path, or (with
//! `--file`/`--expr`) an attribute path into an expression. Resolving one
//! to a value goes through the flake schemas, as Determinate Nix does
//! (`installable-flake.cc`), so the default attribute paths, their order
//! and the error messages are Nix's.
//!
//! An installable is bytes, as Nix's arguments are, and so is the
//! directory relative paths are taken against: a store path can be named
//! through any file name. Flake references and attribute paths are text
//! to brix, and one that is not UTF-8 is refused by name.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;

use bstr::{BStr, BString, ByteSlice};

pub use brix::parse_drv_name;
use brix::schemas::{self, Schemas};
use brix::{
    attr_path_to_string, best_suggestions, format_suggestions, parse_attr_path, scored_suggestions,
    AutoArg, ContextElem, Cursor, Error, Flake, Result, Session,
};
use brix_flake::{parse_flake_ref_with_fragment, FetchSettings, FlakeRef, LockFlags, ParseOptions};
use brix_store::StorePath;
pub use brix_store::{DerivedPath, ExtendedOutputsSpec, OutputsSpec};

/// `ExtendedOutputsSpec::parse`, failing as Nix does: for text (see
/// [`parse_outputs_spec_bytes`]).
pub fn parse_outputs_spec(s: &str) -> Result<(&str, ExtendedOutputsSpec)> {
    let (prefix, spec) = parse_outputs_spec_bytes(s.as_bytes())?;
    // The text before an ASCII `^` (or all of it) is text.
    Ok((&s[..prefix.len()], spec))
}

/// `ExtendedOutputsSpec::parse` on bytes: what precedes the last `^`, and
/// the outputs after it. Output names are store path names (`checkName`),
/// which bytes that are not UTF-8 never are.
pub fn parse_outputs_spec_bytes(s: &[u8]) -> Result<(&[u8], ExtendedOutputsSpec)> {
    let Some(caret) = s.rfind_byte(b'^') else {
        return Ok((s, ExtendedOutputsSpec::Default));
    };
    match OutputsSpec::parse(&s[caret + 1..]) {
        Ok(spec) => Ok((&s[..caret], ExtendedOutputsSpec::Explicit(spec))),
        // `ExtendedOutputsSpec::parse`'s `Error`, the installable quoted
        // as given.
        Err(_) => Err(Error::Usage(
            [b"invalid extended outputs specifier '".as_slice(), s, b"'"]
                .concat()
                .into(),
        )),
    }
}

/// brix's refusal of `value`, one of `what` (`"flake references"`), which
/// brix keeps as text, given in bytes that are not UTF-8.
fn not_utf8(what: &str, value: &[u8]) -> Error {
    Error::Usage(
        [
            format!("brix does not support {what} that are not valid UTF-8, such as '").as_bytes(),
            value,
            b"'",
        ]
        .concat()
        .into(),
    )
}

/// brix's refusal of what needs the current directory as text (flake
/// references, and files named by text relative to it) when its name,
/// `dir`, is not UTF-8.
pub fn cwd_not_utf8(dir: &[u8]) -> Error {
    not_utf8(
        "flake references or file arguments in current directories",
        dir,
    )
}

/// Where an installable's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallableSource {
    /// `flakeref#fragment`.
    Flake {
        flake_ref: FlakeRef,
        fragment: String,
    },
    /// `/nix/store/…` (a derivation or an output path).
    StorePath(StorePath),
    /// An attribute path relative to the `--file`/`--expr` value.
    AttrPath(String),
}

/// A parsed installable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installable {
    pub source: InstallableSource,
    pub outputs: ExtendedOutputsSpec,
    /// The installable as the user wrote it: bytes.
    pub text: BString,
}

impl Installable {
    /// Parses `s` as a store path or a flake reference with fragment,
    /// relative paths against `base_dir`
    /// (`SourceExprCommand::parseInstallables`). Both are bytes, as in
    /// Nix; a flake reference must be text, and so must `base_dir` for
    /// one.
    pub fn parse(
        settings: &FetchSettings,
        store_dir: &brix_store::StoreDir,
        s: impl AsRef<[u8]>,
        base_dir: impl AsRef<[u8]>,
    ) -> Result<Installable> {
        let (s, base_dir) = (s.as_ref(), base_dir.as_ref());
        let (prefix, outputs) = parse_outputs_spec_bytes(s)?;
        if prefix.contains(&b'/') {
            // `InstallableDerivedPath::parse`. Whatever stops the text from
            // being a store path, the flake reference parser gets it next
            // and its error is the one reported (Nix keeps the last
            // exception).
            let path = match &outputs {
                // Without `^`, a symlink chain into the store or a path
                // inside a store path will do.
                ExtendedOutputsSpec::Default => {
                    follow_links_to_store_path(store_dir, prefix, base_dir)
                }
                // With `^`, exactly what is written (`SingleDerivedPath::parse`).
                ExtendedOutputsSpec::Explicit(_) => store_dir.maybe_parse(prefix),
            };
            if let Some(path) = path {
                return Ok(Installable {
                    source: InstallableSource::StorePath(path),
                    outputs,
                    text: s.into(),
                });
            }
        }
        // brix's flake references are text (Nix parses the bytes as a URL,
        // falling back to a path).
        let prefix = std::str::from_utf8(prefix).map_err(|_| not_utf8("flake references", s))?;
        let base_dir = std::str::from_utf8(base_dir).map_err(|_| cwd_not_utf8(base_dir))?;
        let opts = ParseOptions {
            base_dir: Some(base_dir.to_owned()),
            ..ParseOptions::flake()
        };
        let (flake_ref, fragment) = parse_flake_ref_with_fragment(settings, prefix, &opts)?;
        Ok(Installable {
            source: InstallableSource::Flake {
                flake_ref,
                fragment,
            },
            outputs,
            text: s.into(),
        })
    }

    /// `Installable::what`: how Nix names the installable in messages
    /// (`flakeref#fragment`, the store path with any `^outputs`, or the
    /// attribute path, empty for `.`).
    pub fn what(&self, store_dir: &brix_store::StoreDir) -> String {
        match &self.source {
            InstallableSource::Flake {
                flake_ref,
                fragment,
            } => format!("{flake_ref}#{fragment}"),
            InstallableSource::StorePath(path) => match &self.outputs {
                ExtendedOutputsSpec::Default => store_dir.display(path),
                // Parsed from the installable's text: the names are store
                // path names, and so text.
                ExtendedOutputsSpec::Explicit(spec) => {
                    format!(
                        "{}^{}",
                        store_dir.display(path),
                        BStr::new(&spec.to_bytes())
                    )
                }
            },
            InstallableSource::AttrPath(path) => path.clone(),
        }
    }

    /// An attribute path into a `--file`/`--expr` value
    /// (`InstallableAttrPath::parse`: `.` means the value itself). Nix
    /// selects attributes by bytes; brix's attribute paths are text.
    pub fn attr_path(s: impl AsRef<[u8]>) -> Result<Installable> {
        let s = s.as_ref();
        let (prefix, outputs) = parse_outputs_spec_bytes(s)?;
        let prefix = std::str::from_utf8(prefix).map_err(|_| not_utf8("attribute paths", s))?;
        Ok(Installable {
            source: InstallableSource::AttrPath(if prefix == "." {
                String::new()
            } else {
                prefix.to_owned()
            }),
            outputs,
            text: s.into(),
        })
    }
}

/// `Store::followLinksToStorePath`: `path` (absolute, or relative to
/// `base_dir`) with symlinks followed until it lies inside the store, as
/// the store path it lies in (`toStorePath` drops the path within it).
/// Bytes throughout, as Nix's paths are. `None` when it does not end up in
/// the store or names no valid store path, and also when a symlink cannot
/// be read or the chain is 1024 links long: `parseInstallables` tries the
/// text as a flake reference next whatever went wrong here, and reports
/// that parser's error.
fn follow_links_to_store_path(
    store_dir: &brix_store::StoreDir,
    path: &[u8],
    base_dir: &[u8],
) -> Option<StorePath> {
    const MAX_FOLLOW: u32 = 1024;
    // `absPath(path, dir)`: lexical, without resolving symlinks.
    let mut path = crate::commands::absolute_bytes(base_dir, path);
    let mut follows = 0;
    while !store_dir.is_in_store(&path) {
        let fs_path = crate::commands::os_path(path.clone());
        let is_symlink =
            std::fs::symlink_metadata(&fs_path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_symlink {
            break;
        }
        follows += 1;
        if follows >= MAX_FOLLOW {
            return None;
        }
        let target = std::fs::read_link(&fs_path).ok()?;
        let target = target.as_os_str().as_bytes();
        let parent: &[u8] = match path.rfind_byte(b'/') {
            Some(0) | None => b"/",
            Some(i) => &path[..i],
        };
        path = crate::commands::absolute_bytes(parent, target);
    }
    store_dir.store_path_of(&path).ok()
}

/// Resolves installables for one command: caches the flakes it locks and
/// the `--file`/`--expr` root, and knows the command's roles and system.
pub struct Resolver<'s> {
    session: &'s Session,
    /// The value `--file`/`--expr` evaluated to, if given.
    root: Option<Cursor<'s>>,
    auto_args: Vec<(String, AutoArg)>,
    /// What relative paths are taken against (the current directory):
    /// bytes, as Nix's `absPath` has it.
    base_dir: BString,
    /// The system installables default to (`settings.thisSystem`).
    system: String,
    /// `getRoles()`: `nix-build`, `nix-run`, `nix-search`, …
    roles: BTreeSet<String>,
    lock_flags: LockFlags,
    /// `--default-flake-schemas`.
    default_schemas: Option<FlakeRef>,
    flakes: BTreeMap<String, Flake<'s>>,
}

/// An app before its program's dependencies are built (`UnresolvedApp`).
#[derive(Clone, Debug)]
pub struct UnresolvedApp {
    pub program: String,
    pub context: Vec<DerivedPath>,
}

impl<'s> Resolver<'s> {
    pub fn new(
        session: &'s Session,
        base_dir: impl AsRef<[u8]>,
        system: &str,
        roles: impl IntoIterator<Item = &'static str>,
        lock_flags: LockFlags,
    ) -> Resolver<'s> {
        Resolver {
            session,
            root: None,
            auto_args: Vec::new(),
            base_dir: base_dir.as_ref().into(),
            system: system.to_owned(),
            roles: roles.into_iter().map(str::to_owned).collect(),
            lock_flags,
            default_schemas: None,
            flakes: BTreeMap::new(),
        }
    }

    pub fn with_default_schemas(mut self, flake_ref: Option<FlakeRef>) -> Self {
        self.default_schemas = flake_ref;
        self
    }

    /// `--arg`/`--argstr` values for `--file`/`--expr` functions.
    pub fn with_auto_args(mut self, args: Vec<(String, AutoArg)>) -> Self {
        self.auto_args = args;
        self
    }

    /// Evaluates `--expr`, or the source `--file -` reads: bytes, its
    /// relative paths against the base directory
    /// (`parseExprFromString`, `parseStdin`).
    pub fn with_expr(mut self, expr: impl AsRef<[u8]>) -> Result<Self> {
        self.root = Some(
            self.session
                .eval_expr_cursor(expr.as_ref(), &self.base_dir)?,
        );
        Ok(self)
    }

    /// Evaluates `--file`, an absolute path in bytes (already made
    /// readable through the host).
    pub fn with_file(mut self, path: impl AsRef<[u8]>) -> Result<Self> {
        self.root = Some(self.session.eval_file_cursor(path.as_ref())?);
        Ok(self)
    }

    pub fn session(&self) -> &'s Session {
        self.session
    }

    /// Parses `strs` (bytes; text will do) the way the command's mode
    /// dictates: attribute paths when `--file`/`--expr` was given,
    /// installables otherwise.
    pub fn parse(&self, strs: &[impl AsRef<[u8]>]) -> Result<Vec<Installable>> {
        let settings = self.session.fetch_settings();
        let store_dir = self.session.config().store_dir().clone();
        strs.iter()
            .map(|s| {
                if self.root.is_some() {
                    Installable::attr_path(s)
                } else {
                    Installable::parse(&settings, &store_dir, s, &self.base_dir)
                }
            })
            .collect()
    }

    /// The flake behind a flake installable, locked once per resolver.
    pub fn flake(&mut self, flake_ref: &FlakeRef) -> Result<&Flake<'s>> {
        let key = flake_ref.to_string();
        if !self.flakes.contains_key(&key) {
            // Like Nix with lazy trees, the top-level flake's own reference
            // is not hashed unless something needs it.
            let flake = self
                .session
                .flake_with_flags(flake_ref, &self.lock_flags, false)?;
            self.flakes.insert(key.clone(), flake);
        }
        Ok(&self.flakes[&key])
    }

    /// `InstallableFlake::getAttrPaths` through the flake's schemas.
    fn flake_attr_paths(
        &self,
        schemas: &Schemas,
        fragment: &str,
        use_default_attr_path: bool,
    ) -> Result<Vec<Vec<String>>> {
        if let Some(exact) = fragment.strip_prefix('.') {
            return Ok(vec![parse_attr_path(exact)?]);
        }
        let parsed = parse_attr_path(fragment)?;
        let mut attr_paths: Vec<Vec<String>> = Vec::new();
        for schema_name in schemas.sorted_names() {
            let schema = &schemas.map[&schema_name];
            for role in &self.roles {
                if !schema.roles.contains(role) {
                    continue;
                }
                let mut attr_path = vec![schema_name.clone()];
                if schema.append_system {
                    attr_path.push(self.system.clone());
                }
                if use_default_attr_path && parsed.is_empty() {
                    if let Some(default) = &schema.default_attr_path {
                        let mut p = attr_path.clone();
                        p.extend(default.iter().cloned());
                        attr_paths.push(p);
                    }
                } else {
                    let mut p = attr_path.clone();
                    p.extend(parsed.iter().cloned());
                    attr_paths.push(p);
                }
            }
        }
        if !parsed.is_empty() {
            attr_paths.push(parsed);
        }
        Ok(attr_paths)
    }

    /// `InstallableFlake::getCursors`: every attribute path that exists,
    /// or Nix's "does not provide attribute" error with suggestions.
    pub fn flake_cursors(
        &mut self,
        flake_ref: &FlakeRef,
        fragment: &str,
        use_default_attr_path: bool,
    ) -> Result<Vec<Cursor<'s>>> {
        let default_schemas = self.default_schemas.clone();
        let session = self.session;
        let flake = self.flake(flake_ref)?;
        let root = schemas::schema_root(session, flake, default_schemas.as_ref())?;
        let inventory = root.get_attr("inventory")?;
        // Cursors into the outputs report attribute paths relative to the
        // outputs set, as Nix's `cleanupAttrPath` arranges.
        let outputs = root.get_attr("outputs")?.renamed(Vec::new());
        let schemas = Schemas::from_inventory(&inventory)?;
        let attr_paths = self.flake_attr_paths(&schemas, fragment, use_default_attr_path)?;
        if attr_paths.is_empty() {
            return Err(Error::Usage(format!(
                "Flake '{}' does not have any schema that provides a default output for the role(s) {}.",
                flake_ref,
                self.roles.iter().cloned().collect::<Vec<_>>().join(", ")
            ).into()));
        }
        let mut res = Vec::new();
        // Nix's `Suggestions`: a set ordered by distance, then name.
        let mut suggestions: BTreeSet<(usize, String)> = BTreeSet::new();
        for attr_path in &attr_paths {
            match find_with_suggestions(&outputs, attr_path) {
                Ok(Some(c)) => res.push(c),
                Ok(None) => {}
                Err(FindError::Missing(s)) => suggestions.extend(s),
                // A type error along the way (e.g. selecting into a
                // string): try the next attribute path, as Nix does.
                Err(FindError::Eval(e)) => {
                    if matches!(e, Error::Eval { .. }) {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        if res.is_empty() {
            let shown: Vec<String> = attr_paths
                .iter()
                .map(|p| format!("'{}'", attr_path_to_string(p)))
                .collect();
            let shown = match shown.len() {
                0 => String::new(),
                1 => shown[0].clone(),
                n => format!("{} or {}", shown[..n - 1].join(", "), shown[n - 1]),
            };
            let mut msg = format!("flake '{flake_ref}' does not provide attribute {shown}");
            // `Suggestions::trim`: the five closest (each list holds only
            // names close enough).
            let suggestions: Vec<String> =
                suggestions.into_iter().take(5).map(|(_, n)| n).collect();
            if !suggestions.is_empty() {
                msg.push_str(&format!(
                    "\n       Did you mean {}?",
                    format_suggestions(&suggestions)
                ));
            }
            return Err(Error::Usage(msg.into()));
        }
        Ok(res)
    }

    /// `Installable::getCursor`: the first attribute path that exists.
    pub fn cursor(&mut self, installable: &Installable) -> Result<Cursor<'s>> {
        let mut all = self.cursors(installable, true)?;
        Ok(all.remove(0))
    }

    /// `InstallableValue::getCursors`.
    pub fn cursors(
        &mut self,
        installable: &Installable,
        use_default_attr_path: bool,
    ) -> Result<Vec<Cursor<'s>>> {
        match &installable.source {
            InstallableSource::Flake {
                flake_ref,
                fragment,
            } => self.flake_cursors(flake_ref, fragment, use_default_attr_path),
            InstallableSource::AttrPath(path) => {
                let root = self.root.clone().ok_or_else(|| {
                    Error::Usage("attribute paths need '--file' or '--expr'".into())
                })?;
                Ok(vec![self.find_along_attr_path(&root, path)?])
            }
            InstallableSource::StorePath(p) => Err(Error::Usage(
                format!(
                    "'{}' is a store path, not a Nix value",
                    self.session.config().store_dir().display(p)
                )
                .into(),
            )),
        }
    }

    /// `findAlongAttrPath` with auto-called functions: the value at
    /// `attr_path` in `root`, calling functions with the `--arg`s on the way.
    pub fn find_along_attr_path(&self, root: &Cursor<'s>, attr_path: &str) -> Result<Cursor<'s>> {
        let tokens = parse_attr_path(attr_path)?;
        let mut cur = root.clone();
        let mut attr_so_far: Vec<String> = Vec::new();
        for token in tokens {
            cur = self
                .session
                .auto_call(&cur, &self.auto_args, &self.base_dir)?;
            attr_so_far.push(token.clone());
            let next = if cur.is_attrs()? {
                match cur.maybe_get_attr(&token)? {
                    Some(c) => c,
                    None => {
                        let names = cur.get_attrs()?;
                        let suggestions =
                            best_suggestions(names.iter().map(String::as_str), &token);
                        let mut msg = format!(
                            "attribute '{}' in selection path '{}' not found",
                            token,
                            attr_path_to_string(&attr_so_far)
                        );
                        if !suggestions.is_empty() {
                            msg.push_str(&format!(
                                "\n       Did you mean {}?",
                                format_suggestions(&suggestions)
                            ));
                        }
                        return Err(Error::Usage(msg.into()));
                    }
                }
            } else if cur.value_type()? == brix::ValueType::List {
                let index: usize = token.parse().map_err(|_| {
                    Error::Usage(format!(
                        "attribute selection path '{attr_path}' does not match an attribute set or list"
                    ).into())
                })?;
                let elems = cur.get_list()?;
                elems.into_iter().nth(index).ok_or_else(|| {
                    Error::Usage(
                        format!(
                            "list index {index} in selection path '{attr_path}' is out of range"
                        )
                        .into(),
                    )
                })?
            } else {
                return Err(Error::Usage(format!(
                    "the expression selected by the selection path '{}' should be a set or list but is {}",
                    attr_path_to_string(&attr_so_far),
                    cur.show_type()?
                ).into()));
            };
            cur = next;
        }
        Ok(cur)
    }

    /// `Installable::toDerivedPaths`: what building the installable means.
    pub fn derived_paths(&mut self, installable: &Installable) -> Result<Vec<DerivedPath>> {
        if let InstallableSource::StorePath(path) = &installable.source {
            // `InstallableDerivedPath::parse`: without `^` the path itself,
            // a `.drv` included; with it, outputs of the derivation.
            // Whether the path is a derivation is found out when it is read
            // (`Store::readDerivation`).
            let derived = match &installable.outputs {
                ExtendedOutputsSpec::Default => DerivedPath::Opaque(path.clone()),
                ExtendedOutputsSpec::Explicit(spec) => DerivedPath::Built {
                    drv_path: path.clone(),
                    outputs: spec.clone(),
                },
            };
            return Ok(vec![derived]);
        }
        let cursor = self.cursor(installable)?;
        match &installable.source {
            InstallableSource::Flake { .. } => {
                let attr_path = cursor.attr_path_str();
                if !cursor.is_derivation()? {
                    // A path or a string with context would also do
                    // (`trySinglePathToDerivedPaths`).
                    let error_ctx =
                        format!("while evaluating the flake output attribute '{attr_path}'");
                    if let Some(dp) = self.single_path(&cursor, false, &error_ctx)? {
                        return Ok(vec![dp]);
                    }
                    let mut msg = format!(
                        "expected flake output attribute '{}' to be a derivation or path but found {}: ",
                        attr_path,
                        cursor.show_type()?,
                    )
                    .into_bytes();
                    msg.extend_from_slice(&cursor.print_for_error()?);
                    return Err(Error::Usage(msg.into()));
                }
                let drv_path = cursor.force_derivation()?;
                let outputs = match &installable.outputs {
                    ExtendedOutputsSpec::Explicit(spec) => spec.clone(),
                    ExtendedOutputsSpec::Default => {
                        // Output names are bytes, as Nix's are.
                        let mut to_install: BTreeSet<BString> = BTreeSet::new();
                        if let Some(specified) = cursor.maybe_get_attr("outputSpecified")? {
                            if specified.get_bool()? {
                                if let Some(name) = cursor.maybe_get_attr("outputName")? {
                                    to_install.insert(name.get_bytes_with_context()?.0.into());
                                }
                            }
                        } else if let Some(meta) = cursor.maybe_get_attr("meta")? {
                            if let Some(outs) = meta.maybe_get_attr("outputsToInstall")? {
                                for o in outs.get_list_of_bytes()? {
                                    to_install.insert(o.into());
                                }
                            }
                        }
                        if to_install.is_empty() {
                            to_install.insert("out".into());
                        }
                        OutputsSpec::Names(to_install)
                    }
                };
                // Nix reads `meta.priority` (for profiles) whatever the command:
                // one that is not an integer fails it.
                if cursor.maybe_get_attr("outputSpecified")?.is_none() {
                    if let Some(meta) = cursor.maybe_get_attr("meta")? {
                        if let Some(priority) = meta.maybe_get_attr("priority")? {
                            priority.get_int()?;
                        }
                    }
                }
                Ok(vec![DerivedPath::Built { drv_path, outputs }])
            }
            InstallableSource::AttrPath(path) => {
                let error_ctx = format!("while evaluating the attribute '{path}'");
                // Under the attribute's position (`findAlongAttrPath`'s).
                if let Some(dp) = self.single_path(&cursor, true, &error_ctx)? {
                    return Ok(vec![dp]);
                }
                // `getDerivations`: the value, or the derivations it contains.
                let mut by_drv: BTreeMap<StorePath, OutputsSpec> = BTreeMap::new();
                let mut found = Vec::new();
                self.collect_derivations(&cursor, &mut found)?;
                if found.is_empty() {
                    return Err(Error::Usage(
                        [b"'".as_slice(), &installable.text, b"' is not a derivation"]
                            .concat()
                            .into(),
                    ));
                }
                for drv in found {
                    let drv_path = drv.force_derivation()?;
                    let outputs = match &installable.outputs {
                        ExtendedOutputsSpec::Explicit(spec) => spec.clone(),
                        ExtendedOutputsSpec::Default => {
                            // Output names are bytes, as Nix's are.
                            let mut names: BTreeSet<BString> = BTreeSet::new();
                            if let Some(meta) = drv.maybe_get_attr("meta")? {
                                if let Some(outs) = meta.maybe_get_attr("outputsToInstall")? {
                                    for o in outs.get_list_of_bytes()? {
                                        names.insert(o.into());
                                    }
                                }
                            }
                            if names.is_empty() {
                                if let Some(name) = drv.maybe_get_attr("outputName")? {
                                    names.insert(name.get_bytes_with_context()?.0.into());
                                }
                            }
                            if names.is_empty() {
                                names.insert("out".into());
                            }
                            OutputsSpec::Names(names)
                        }
                    };
                    by_drv
                        .entry(drv_path)
                        .and_modify(|o| *o = o.union(&outputs))
                        .or_insert(outputs);
                }
                Ok(by_drv
                    .into_iter()
                    .map(|(drv_path, outputs)| DerivedPath::Built { drv_path, outputs })
                    .collect())
            }
            InstallableSource::StorePath(_) => unreachable!("handled above"),
        }
    }

    /// `trySinglePathToDerivedPaths`: a path value denotes the store path
    /// it is copied to, a string the one store path or derivation output
    /// its context names (`coerceToSingleDerivedPath`), with Nix's errors
    /// under the trace `error_ctx`, at where the value was defined when
    /// `trace_at_definition`; `None` for any other value.
    fn single_path(
        &self,
        cursor: &Cursor<'s>,
        trace_at_definition: bool,
        error_ctx: &str,
    ) -> Result<Option<DerivedPath>> {
        use brix::ValueType;
        let store_dir = self.session.config().store_dir().clone();
        match cursor.value_type()? {
            // Copied to the store (here: the virtual store, uploaded when a
            // build needs it).
            ValueType::Path => Ok(cursor.fetch_to_store()?.map(DerivedPath::Opaque)),
            ValueType::String => {
                // The string's bytes, as the messages quote them.
                let (s, mut context) = cursor.get_bytes_with_context()?;
                let fail = |what: &str, rest: &[u8]| {
                    let msg = [what.as_bytes(), b" '", &s, b"' ", rest].concat();
                    if trace_at_definition {
                        cursor.error_with_trace(msg, error_ctx)
                    } else {
                        self.session.error_with_trace(msg, error_ctx)
                    }
                };
                if context.len() != 1 {
                    return Err(fail(
                        "string",
                        format!(
                            "has {} entries in its context. It should only have exactly one entry",
                            context.len()
                        )
                        .as_bytes(),
                    ));
                }
                let (derived, expected) = match context.remove(0) {
                    ContextElem::Opaque(path) => {
                        let expected = store_dir.display(&path);
                        if s != expected.as_bytes() {
                            return Err(fail(
                                "path string",
                                format!("has context with the different path '{expected}'")
                                    .as_bytes(),
                            ));
                        }
                        (DerivedPath::Opaque(path), expected)
                    }
                    ContextElem::Built { drv, output } => {
                        let expected = self.output_string(&drv, &output)?;
                        if s != expected.as_bytes() {
                            // The output name's bytes as they are.
                            let rest = [
                                b"has context with the output '".as_slice(),
                                output.as_slice(),
                                format!(
                                    "' from derivation '{}', but the string is not the right placeholder for this derivation output. It should be '{expected}'",
                                    store_dir.display(&drv)
                                )
                                .as_bytes(),
                            ]
                            .concat();
                            return Err(fail("string", &rest));
                        }
                        let outputs = OutputsSpec::Names([output].into_iter().collect());
                        (
                            DerivedPath::Built {
                                drv_path: drv,
                                outputs,
                            },
                            expected,
                        )
                    }
                    ContextElem::DrvDeep(_) => {
                        return Err(fail(
                            "string",
                            b"has a context which refers to a complete source and binary closure. This is not supported at this time",
                        ))
                    }
                    ContextElem::Path(_) => {
                        return Err(fail("string", b"has no context"))
                    }
                };
                debug_assert_eq!(s, expected.as_bytes());
                Ok(Some(derived))
            }
            _ => Ok(None),
        }
    }

    /// `mkSingleDerivedPathStringRaw` for output `output` (bytes) of
    /// `drv`: its store path when the derivation fixes it, else the
    /// placeholder that stands for it.
    fn output_string(&self, drv: &StorePath, output: &[u8]) -> Result<String> {
        let store_dir = self.session.config().store_dir();
        let derivation = self.session.read_derivation(drv)?;
        let out = derivation.outputs.get(output).ok_or_else(|| {
            Error::Store(
                [
                    format!(
                        "derivation '{}' does not have output '",
                        store_dir.display(drv)
                    )
                    .as_bytes(),
                    output,
                    b"'",
                ]
                .concat()
                .into(),
            )
        })?;
        let path = out.path(store_dir, &derivation.name, output)?;
        match path {
            Some(p) => Ok(store_dir.display(&p)),
            None => {
                brix_store::derivation::downstream_placeholder(drv, output).map_err(Error::from)
            }
        }
    }

    /// `getDerivations`: derivations in `cursor` (itself, the attributes
    /// of a set, recursing into `recurseForDerivations` sets and lists).
    fn collect_derivations(&self, cursor: &Cursor<'s>, out: &mut Vec<Cursor<'s>>) -> Result<()> {
        use brix::ValueType;
        let cur = self
            .session
            .auto_call(cursor, &self.auto_args, &self.base_dir)?;
        if cur.is_derivation()? {
            out.push(cur);
            return Ok(());
        }
        match cur.value_type()? {
            ValueType::Attrs => {
                for name in cur.get_attrs()? {
                    let child = cur.get_attr(&name)?;
                    let child = self
                        .session
                        .auto_call(&child, &self.auto_args, &self.base_dir)?;
                    if child.is_derivation()? {
                        out.push(child);
                    } else if child.value_type()? == ValueType::Attrs {
                        let recurse = match child.maybe_get_attr("recurseForDerivations")? {
                            Some(r) => r.get_bool()?,
                            None => false,
                        };
                        if recurse {
                            self.collect_derivations(&child, out)?;
                        }
                    }
                }
                Ok(())
            }
            ValueType::List => {
                for elem in cur.get_list()? {
                    self.collect_derivations(&elem, out)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `Installable::toDerivations`: the derivation paths behind
    /// `installables`; a plain store path must be a `.drv`.
    pub fn derivations(&mut self, installables: &[Installable]) -> Result<BTreeSet<StorePath>> {
        let mut out = BTreeSet::new();
        for i in installables {
            for d in self.derived_paths(i)? {
                match d {
                    DerivedPath::Built { drv_path, .. } => {
                        out.insert(drv_path);
                    }
                    DerivedPath::Opaque(p) if p.is_derivation() => {
                        out.insert(p);
                    }
                    DerivedPath::Opaque(_) => {
                        // The installable as written, bytes.
                        return Err(Error::Usage(
                            [
                                b"argument '".as_slice(),
                                &i.text,
                                b"' did not evaluate to a derivation",
                            ]
                            .concat()
                            .into(),
                        ));
                    }
                }
            }
        }
        Ok(out)
    }

    /// `InstallableValue::toApp`: what `nix run` executes.
    pub fn app(&mut self, installable: &Installable) -> Result<UnresolvedApp> {
        let cursor = self.cursor(installable)?;
        let attr_path = cursor.attr_path();
        let ty = cursor.get_attr("type")?.get_string()?;
        let expected = if attr_path
            .first()
            .is_some_and(|a| a == "apps" || a == "defaultApp")
        {
            "app"
        } else {
            "derivation"
        };
        if ty != expected {
            return Err(Error::Usage(
                format!(
                    "attribute '{}' should have type '{}'",
                    cursor.attr_path_str(),
                    expected
                )
                .into(),
            ));
        }
        if ty == "app" {
            let (program, context) = cursor.get_attr("program")?.get_string_with_context()?;
            let deps = context
                .into_iter()
                .map(|elem| match elem {
                    ContextElem::Built { drv, output } => Ok(DerivedPath::Built {
                        drv_path: drv,
                        outputs: OutputsSpec::Names([output].into_iter().collect()),
                    }),
                    // All outputs of the derivation.
                    ContextElem::DrvDeep(drv) => Ok(DerivedPath::Built {
                        drv_path: drv,
                        outputs: OutputsSpec::All,
                    }),
                    ContextElem::Opaque(path) => Ok(DerivedPath::Opaque(path)),
                    ContextElem::Path(_) => Err(Error::Usage(
                        "'program' attribute of an 'app' output cannot have no context".into(),
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(UnresolvedApp {
                program,
                context: deps,
            });
        }
        let drv_path = cursor.force_derivation()?;
        let out_path = cursor.get_attr("outPath")?.get_string()?;
        // Bytes, as an output name is.
        let output_name = BString::from(cursor.get_attr("outputName")?.get_bytes_with_context()?.0);
        let name = cursor.get_attr("name")?.get_string()?;
        let main_program = match cursor.maybe_get_attr("meta")? {
            Some(meta) => match meta.maybe_get_attr("mainProgram")? {
                Some(m) => Some(m.get_string()?),
                None => None,
            },
            None => None,
        };
        let main_program = match main_program {
            Some(m) => m,
            None => match cursor.maybe_get_attr("pname")? {
                Some(p) => p.get_string()?,
                None => parse_drv_name(&name).0.to_owned(),
            },
        };
        Ok(UnresolvedApp {
            program: format!("{out_path}/bin/{main_program}"),
            context: vec![DerivedPath::Built {
                drv_path,
                outputs: OutputsSpec::Names([output_name].into_iter().collect()),
            }],
        })
    }
}

enum FindError {
    /// The attribute path does not exist; suggestions for its first
    /// missing component, with their distances.
    Missing(Vec<(usize, String)>),
    Eval(Error),
}

/// `findAlongAttrPath` on a cursor, collecting suggestions for the first
/// missing component (what `AttrCursor::findAlongAttrPath` returns).
fn find_with_suggestions<'s>(
    root: &Cursor<'s>,
    attr_path: &[String],
) -> std::result::Result<Option<Cursor<'s>>, FindError> {
    let mut cur = root.clone();
    for component in attr_path {
        let is_attrs = cur.is_attrs().map_err(FindError::Eval)?;
        if !is_attrs {
            return Err(FindError::Eval(Error::Eval {
                message: format!("'{}' is not an attribute set", cur.attr_path_str()).into_bytes(),
                exit_status: 1,
                usage: false,
            }));
        }
        match cur.maybe_get_attr(component).map_err(FindError::Eval)? {
            Some(c) => cur = c,
            None => {
                let names = cur.get_attrs().map_err(FindError::Eval)?;
                return Err(FindError::Missing(scored_suggestions(
                    names.iter().map(String::as_str),
                    component,
                )));
            }
        }
    }
    Ok(Some(cur))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_installables() {
        let settings = FetchSettings::default();
        let store_dir = brix_store::StoreDir::default();
        let i = Installable::parse(
            &settings,
            &store_dir,
            "/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello-2.12.2.drv^out",
            "/",
        )
        .unwrap();
        assert!(matches!(i.source, InstallableSource::StorePath(_)));
        assert!(matches!(i.outputs, ExtendedOutputsSpec::Explicit(_)));
        let f =
            Installable::parse(&settings, &store_dir, "github:NixOS/nixpkgs#hello", "/").unwrap();
        assert!(
            matches!(f.source, InstallableSource::Flake { ref fragment, .. } if fragment == "hello")
        );
    }

    const HELLO: &str = "/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello-2.12.2";

    fn parse_in(dir: &std::path::Path, s: impl AsRef<[u8]>) -> Result<Installable> {
        Installable::parse(
            &FetchSettings::default(),
            &brix_store::StoreDir::default(),
            s,
            dir.as_os_str().as_bytes(),
        )
    }

    fn store_path_of(i: &Installable) -> &str {
        match &i.source {
            InstallableSource::StorePath(p) => p.as_str(),
            other => panic!("not a store path: {other:?}"),
        }
    }

    /// `followLinksToStorePath`: a symlink chain into the store, relative
    /// or absolute, or a path inside a store path, is that store path.
    #[test]
    fn store_path_installables_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::os::unix::fs::symlink(HELLO, base.join("result")).unwrap();
        std::os::unix::fs::symlink("result", base.join("again")).unwrap();
        std::fs::create_dir(base.join("sub")).unwrap();
        std::os::unix::fs::symlink("../again", base.join("sub/up")).unwrap();
        let hello = &HELLO["/nix/store/".len()..];
        for text in [
            "./result".to_owned(),
            "./again".to_owned(),
            "sub/up".to_owned(),
            format!("{}/sub/../result", base.display()),
            format!("{HELLO}/bin/hello"),
            format!("{HELLO}/bin/../share"),
        ] {
            let i = parse_in(base, &text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(store_path_of(&i), hello, "{text}");
            assert_eq!(i.outputs, ExtendedOutputsSpec::Default, "{text}");
            assert_eq!(i.text, text);
        }
        // With `^`, only the store path itself.
        let i = parse_in(base, format!("{HELLO}^out")).unwrap();
        assert_eq!(store_path_of(&i), hello);
        for text in ["./result^out", &format!("{HELLO}/bin/hello^out")] {
            assert!(
                !matches!(
                    parse_in(base, text).map(|i| i.source),
                    Ok(InstallableSource::StorePath(_))
                ),
                "{text}"
            );
        }
    }

    /// The installable and the directory are bytes, as Nix's
    /// `followLinksToStorePath` takes them: a store path is named through
    /// file names that are not UTF-8 (`nix eval ./res$'\xe9'`, a symlink
    /// to `hello`, names `hello`'s store path).
    #[test]
    fn store_paths_named_through_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(std::ffi::OsStr::from_bytes(b"d\xe9"));
        std::fs::create_dir(&base).unwrap();
        let link = std::ffi::OsStr::from_bytes(b"res\xe9");
        std::os::unix::fs::symlink(HELLO, base.join(link)).unwrap();
        std::os::unix::fs::symlink(link, base.join("again")).unwrap();
        let hello = &HELLO["/nix/store/".len()..];
        let inside = [HELLO.as_bytes(), b"/caf\xe9"].concat();
        let absolute = [base.as_os_str().as_bytes(), b"/res\xe9"].concat();
        let cases: [&[u8]; 4] = [b"./res\xe9", b"./again", &inside, &absolute];
        for text in cases {
            let i = parse_in(&base, text).unwrap_or_else(|e| panic!("{}: {e}", text.as_bstr()));
            assert_eq!(store_path_of(&i), hello, "{}", text.as_bstr());
            assert_eq!(i.text, text);
        }
    }

    /// Flake references and attribute paths are text to brix: one that is
    /// not UTF-8, or a flake reference while the current directory's name
    /// is not, is refused by name. Output names after `^` are never bytes
    /// that are not UTF-8, as Nix finds (`invalid extended outputs
    /// specifier`, quoting the installable's bytes).
    #[test]
    fn what_brix_keeps_as_text_is_refused_by_name() {
        let refused = |r: Result<Installable>| match r {
            Err(Error::Usage(message)) => message,
            other => panic!("not refused: {other:?}"),
        };
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            refused(parse_in(dir.path(), b"./a\xffb#x")),
            &b"brix does not support flake references that are not valid UTF-8, such as './a\xffb#x'"[..]
        );
        assert_eq!(
            refused(parse_in(dir.path(), b"nixpkgs#he\xfflo")),
            &b"brix does not support flake references that are not valid UTF-8, such as 'nixpkgs#he\xfflo'"[..]
        );
        assert_eq!(
            refused(Installable::parse(
                &FetchSettings::default(),
                &brix_store::StoreDir::default(),
                "nixpkgs#hello",
                b"/tmp/d\xe9",
            )),
            &b"brix does not support flake references or file arguments in current directories that are not valid UTF-8, such as '/tmp/d\xe9'"[..]
        );
        assert_eq!(
            refused(Installable::attr_path(b"a.\xff")),
            &b"brix does not support attribute paths that are not valid UTF-8, such as 'a.\xff'"[..]
        );
        assert_eq!(
            refused(parse_in(dir.path(), b"foo^o\xff")),
            &b"invalid extended outputs specifier 'foo^o\xff'"[..]
        );
        assert_eq!(
            refused(Installable::attr_path(b"a^\xff")),
            &b"invalid extended outputs specifier 'a^\xff'"[..]
        );
        // The text before `^` is kept as it was.
        assert_eq!(parse_outputs_spec("caf\u{e9}^out").unwrap().0, "caf\u{e9}");
    }

    /// What does not lead into the store is read as a flake reference,
    /// and that parser's error is the one reported.
    #[test]
    fn non_store_paths_are_flake_references() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // A symlink cycle, a symlink out of the store and a plain file.
        std::os::unix::fs::symlink("b", base.join("a")).unwrap();
        std::os::unix::fs::symlink("a", base.join("b")).unwrap();
        std::os::unix::fs::symlink("/etc", base.join("etc")).unwrap();
        std::fs::write(base.join("file"), "").unwrap();
        for text in ["./a", "./etc", "./file", "/nix/store/not-a-store-path"] {
            let err = parse_in(base, text).unwrap_err();
            assert!(matches!(err, Error::Flake(_)), "{text}: {err:?}");
        }
        // The store directory itself is not in the store.
        let err = parse_in(base, "/nix/store").unwrap_err();
        assert!(matches!(err, Error::Flake(_)), "{err:?}");
    }

    /// Without `^`, a `.drv` denotes the file itself (`DerivedPath::Opaque`),
    /// as in `nix path-info /nix/store/…drv`; with it, its outputs.
    #[test]
    fn a_bare_drv_is_opaque() {
        let mut config = brix::Config::default();
        config.fetch.enabled = false;
        config.compiled.enabled = false;
        let session = Session::new(config).unwrap();
        let mut resolver = Resolver::new(&session, "/", "x86_64-linux", [], LockFlags::pure());
        let drv = format!("{HELLO}.drv");
        let drv_path = StorePath::new(&drv["/nix/store/".len()..]).unwrap();
        let derived = |r: &mut Resolver<'_>, text: &str| -> Vec<DerivedPath> {
            let i = r.parse(&[text.to_owned()]).unwrap().remove(0);
            r.derived_paths(&i).unwrap()
        };
        assert_eq!(
            derived(&mut resolver, &drv),
            [DerivedPath::Opaque(drv_path.clone())]
        );
        assert_eq!(
            derived(&mut resolver, &format!("{drv}^out")),
            [DerivedPath::Built {
                drv_path: drv_path.clone(),
                outputs: OutputsSpec::Names([BString::from("out")].into_iter().collect()),
            }]
        );
        let installables = resolver.parse(std::slice::from_ref(&drv)).unwrap();
        assert_eq!(
            resolver.derivations(&installables).unwrap(),
            [drv_path].into_iter().collect()
        );
    }

    /// An app's `program` context, typed: each element is what `nix run`
    /// builds or fetches before running it.
    #[test]
    fn app_context_is_typed() {
        let mut config = brix::Config::default();
        config.fetch.enabled = false;
        config.compiled.enabled = false;
        let session = Session::new(config).unwrap();
        let drv = r#"(derivation { name = "x"; system = "x"; builder = "/bin/sh"; })"#;
        let expr = format!(
            r#"{{ apps.x = {{ type = "app"; program = "${{{drv}}}/bin/x ${{builtins.unsafeDiscardOutputDependency {drv}.drvPath}} ${{{drv}.drvPath}}"; }}; }}"#
        );
        let mut resolver = Resolver::new(&session, "/", "x86_64-linux", [], LockFlags::pure())
            .with_expr(&expr)
            .unwrap();
        let installable = resolver.parse(&["apps.x".to_owned()]).unwrap().remove(0);
        let app = resolver.app(&installable).unwrap();
        let drv_path = StorePath::new("safnr1ywsblyl5qfbca0178m104hzxaw-x.drv").unwrap();
        let mut context = app.context;
        context.sort();
        assert_eq!(
            context,
            [
                DerivedPath::Opaque(drv_path.clone()),
                DerivedPath::Built {
                    drv_path: drv_path.clone(),
                    outputs: OutputsSpec::All,
                },
                DerivedPath::Built {
                    drv_path,
                    outputs: OutputsSpec::Names([BString::from("out")].into_iter().collect()),
                },
            ]
        );
        assert!(app
            .program
            .starts_with("/nix/store/wrjy0zpx12zvynnga8wrzngbbg6yq3vr-x/bin/x "));
    }
}
