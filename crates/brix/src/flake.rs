//! Flakes on top of the evaluator: reading `flake.nix`, locking through
//! [`brix_flake::lock_flake`], and `callFlake`.
//!
//! brix-flake knows nothing about Nix values, so this module supplies the
//! two things it needs from an evaluator: a [`LockDeps`] implementation
//! that fetches trees and reads their `flake.nix`, and the evaluation of
//! `call-flake.nix`'s logic (brix's own expression, see
//! [`crate::callflake`], with the semantics documented in
//! `brix_flake::call_flake`).

use std::cell::RefCell;
use std::collections::BTreeMap;

use brix_eval::ir::PathAnchor;
use brix_eval::managed_api::{GcCallContext, GcCallValue, GcView};
use brix_eval::value::SrcPos;
use brix_eval::{ErrorKind, Evaluator};
use brix_flake::flake::{apply_self_attrs, FlakePos};
use brix_flake::{
    lookup_in_registries, parse_flake_metadata, Attrs, FetchedInput, Flake, FlakeDir, FlakeRef,
    FlakeValue, Input, LockDeps, LockFlags, LockWrite, LockedFlake, UseRegistries,
};
use brix_store::{CanonPath, StorePath};

use crate::error::{Error, Result};
use crate::host::NativeHost;

/// Where a fetched tree is mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRoot {
    /// At its store path.
    Store(StorePath),
    /// Lazily, at a placeholder: its store path is computed when needed
    /// (see `brix_eval::lazy_tree`).
    Lazy {
        id: brix_eval::LazyTreeId,
        placeholder: String,
    },
}

impl TreeRoot {
    /// The absolute path the tree's files are read at.
    pub fn mount_path(&self, store_dir: &brix_store::StoreDir) -> String {
        match self {
            TreeRoot::Store(sp) => store_dir.display(sp),
            TreeRoot::Lazy { placeholder, .. } => placeholder.clone(),
        }
    }

    /// The store path of a tree that must be lockable: such a fetch is
    /// never mounted lazily.
    pub fn lockable_store_path(&self) -> &StorePath {
        match self {
            TreeRoot::Store(sp) => sp,
            TreeRoot::Lazy { .. } => unreachable!("a lockable tree is mounted at its store path"),
        }
    }
}

/// A fetched tree as the lock algorithm sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeHandle {
    pub root: TreeRoot,
    /// The locked input the tree came from.
    pub input: Input,
}

/// [`LockDeps`] backed by the evaluator and the host.
pub(crate) struct Deps<'e, 'call, 'a, 'h> {
    pub cx: RefCell<&'e mut GcCallContext<'call, 'a, 'h>>,
    show_trace: bool,
    pub host: &'e NativeHost,
    /// The evaluation error behind the last `brix_flake::Error` this
    /// returned, so callers inside the evaluator can rethrow it intact.
    pub eval_error: RefCell<Option<brix_eval::Error>>,
}

impl<'e, 'call, 'a, 'h> Deps<'e, 'call, 'a, 'h> {
    pub fn new(cx: &'e mut GcCallContext<'call, 'a, 'h>, host: &'e NativeHost) -> Self {
        let show_trace = cx.services().config.show_trace;
        Deps {
            cx: RefCell::new(cx),
            show_trace,
            host,
            eval_error: RefCell::new(None),
        }
    }

    fn flake_err(&self, e: Error) -> brix_flake::Error {
        match e {
            Error::Flake(f) => f,
            other => brix_flake::Error::Message(other.to_string()),
        }
    }

    /// `fetchOrSubstituteTree`: resolve through the registries if allowed,
    /// fetch, mount. Returns the tree, the resolved input and the extra
    /// registry attributes (`dir`).
    pub(crate) fn fetch_and_mount(
        &self,
        input: &Input,
        use_registries: UseRegistries,
        require_lockable: bool,
    ) -> Result<(TreeHandle, Input, Attrs)> {
        let (resolved, extra) = if input.is_direct() {
            (input.clone(), Attrs::new())
        } else {
            let registries = self.host.registries();
            lookup_in_registries(&registries, input, use_registries)?
        };
        if !require_lockable {
            if let Some(lazy) = self.fetch_lazily(&resolved)? {
                let handle = TreeHandle {
                    root: TreeRoot::Lazy {
                        id: lazy.id,
                        placeholder: lazy.placeholder,
                    },
                    input: lazy.input,
                };
                return Ok((handle, resolved, extra));
            }
        }
        let tree = self.host.fetch_locked(&resolved, require_lockable)?;
        self.cx
            .borrow()
            .services()
            .mount_fetched_tree(&self.host.to_eval_tree(&tree, false)?)
            .map_err(|e| self.record_eval_error(e))?;
        // `mountInput` adds a `narHash` only to a result that must be
        // lockable, and the `path`, `git` and `hg` fetchers set none
        // themselves: a top-level flake that need not be lockable (`nix
        // flake metadata`) is not described by its NAR hash unless its
        // reference gave one. Evaluation still sees it (`callflake`).
        let mut input = tree.input;
        if !require_lockable
            && matches!(input.type_name().ok(), Some("path" | "git" | "hg"))
            && resolved.nar_hash()?.is_none()
        {
            input.attrs.remove("narHash");
        }
        Ok((
            TreeHandle {
                root: TreeRoot::Store(tree.store_path),
                input,
            },
            resolved,
            extra,
        ))
    }

    /// The tree for `input` mounted lazily, if its NAR hash is not known:
    /// fetched without hashing, readable at once, and hashed only when
    /// evaluation needs its store path. `None` for a tree with a known
    /// hash, which is mounted at its store path as usual.
    fn fetch_lazily(&self, input: &Input) -> Result<Option<crate::host::LazyTree>> {
        let fetched = match self.host.fetch_for_mount(input)? {
            crate::host::Fetched::Lazy(lazy) => return Ok(Some(lazy)),
            crate::host::Fetched::Known => return Ok(None),
            crate::host::Fetched::Unhashed(fetched) => fetched,
        };
        let display = crate::host::display_prefix(&fetched.input)?;
        let (id, placeholder) = self.cx.borrow().services().mount_lazy_tree(
            fetched.accessor.clone(),
            &fetched.input.name()?,
            display,
        );
        let lazy = crate::host::LazyTree {
            accessor: fetched.accessor,
            input: fetched.input,
            id,
            // The store directory and the input's name, which are text.
            placeholder: placeholder
                .to_str()
                .expect("invariant: a lazy tree's placeholder is text")
                .to_owned(),
        };
        self.host.remember_lazy(input, lazy.clone());
        Ok(Some(lazy))
    }

    /// The accessor of a tree this session fetched.
    fn tree_accessor(
        &self,
        root: &TreeRoot,
    ) -> Option<std::sync::Arc<dyn brix_store::SourceAccessor>> {
        match root {
            TreeRoot::Store(sp) => self.host.tree(sp).map(|t| t.accessor),
            TreeRoot::Lazy { id, .. } => self.host.lazy_tree(*id).map(|t| t.accessor),
        }
    }

    /// `readFlake`: evaluate `flake.nix` and extract the metadata.
    fn read_flake(
        &self,
        original_ref: FlakeRef,
        resolved_ref: FlakeRef,
        locked_ref: FlakeRef,
        dir: FlakeDir<TreeHandle>,
        lock_root_attr_path: &[String],
    ) -> Result<Flake<TreeHandle>> {
        if self.tree_accessor(&dir.tree.root).is_none() {
            return Err(Error::Store(
                format!(
                    "tree '{}' is not mounted",
                    dir.tree.root.mount_path(self.host.store_dir())
                )
                .into(),
            ));
        }
        // A missing `flake.nix` is reported by evaluating it, with the
        // tree's own message (Nix 2.35 has no separate check).
        let flake_dir_abs = flake_dir_path(self.host, &dir);
        let flake_nix_abs = format!("{flake_dir_abs}/flake.nix");
        let mut cx = self.cx.borrow_mut();
        let flake_path = CanonPath::new(&flake_nix_abs)?;
        let flake_dir_path = CanonPath::new(&flake_dir_abs)?;
        let accessor = cx.root_accessor();
        let dir_value = brix_eval::value::PathVal {
            accessor,
            path: flake_dir_path.as_bytes(),
        };
        let shown = String::from_utf8(cx.services().show_path(&dir_value).into())
            .map_err(|e| Error::Usage(non_utf8_path_message(e.as_bytes()).into()))?;
        let flake_dir = FlakeDirPath {
            accessor,
            shown,
            path: flake_dir_path,
        };
        let value = cx
            .eval_flake_file(&brix_store::SourcePath::new(accessor, flake_path))
            .map_err(|e| self.record_eval_error(e))?;
        let top =
            flake_value(&mut cx, &value, &flake_dir).map_err(|e| self.record_eval_error(e))?;
        let metadata = parse_flake_metadata(
            &self.host.settings,
            &top,
            lock_root_attr_path,
            &resolved_ref.to_string(),
        )
        .map_err(|e| self.record_eval_error(read_flake_error(cx.services(), &e)))?;
        let locked_ref = apply_self_attrs(&locked_ref, &metadata)?;
        Ok(Flake {
            original_ref,
            resolved_ref,
            locked_ref,
            dir,
            metadata,
            force_dirty: false,
        })
    }

    fn record_eval_error(&self, e: brix_eval::Error) -> Error {
        let err = self.host.eval_error(&e, self.show_trace);
        *self.eval_error.borrow_mut() = Some(e);
        err
    }

    /// `getFlake(state, originalRef, useRegistries, requireLockable)`: the
    /// flake at `flake_ref`, fetched and read.
    pub fn get_flake_full(
        &self,
        flake_ref: &FlakeRef,
        use_registries: UseRegistries,
        lock_root_attr_path: &[String],
        require_lockable: bool,
    ) -> Result<Flake<TreeHandle>> {
        let (tree, resolved, extra) =
            self.fetch_and_mount(&flake_ref.input, use_registries, require_lockable)?;
        let subdir = flake_subdir(&extra, flake_ref)?;
        let resolved_ref = FlakeRef::new(resolved, subdir.clone());
        let locked_ref = FlakeRef::new(tree.input.clone(), subdir.clone());
        let flake = self.read_flake(
            flake_ref.clone(),
            resolved_ref.clone(),
            locked_ref.clone(),
            FlakeDir {
                tree,
                subdir: subdir.clone(),
            },
            lock_root_attr_path,
        )?;
        if flake.locked_ref == locked_ref {
            return Ok(flake);
        }
        // `inputs.self.submodules`/`lfs` changed the input (`applySelfAttrs`):
        // the tree is fetched again with them, without the `narHash` of the
        // first, and the flake read again from it.
        let mut input = flake.locked_ref.input.clone();
        input.attrs.remove("narHash");
        let (tree, _, _) = self.fetch_and_mount(&input, UseRegistries::No, require_lockable)?;
        let locked_ref = FlakeRef::new(tree.input.clone(), subdir.clone());
        self.read_flake(
            flake_ref.clone(),
            resolved_ref,
            locked_ref,
            FlakeDir { tree, subdir },
            lock_root_attr_path,
        )
    }
}

/// `getFlake`'s subdirectory: a registry entry's `dir` (in the extra
/// attributes of the lookup) wins over the reference's own.
fn flake_subdir(extra: &Attrs, flake_ref: &FlakeRef) -> brix_flake::Result<String> {
    Ok(match brix_flake::attrs::maybe_get_str_attr(extra, "dir")? {
        Some(dir) => dir.to_owned(),
        None => flake_ref.subdir.clone(),
    })
}

/// The absolute path of a flake directory on the root file system.
pub(crate) fn flake_dir_path(host: &NativeHost, dir: &FlakeDir<TreeHandle>) -> String {
    let base = dir.tree.root.mount_path(host.store_dir());
    if dir.subdir.is_empty() {
        base
    } else {
        format!("{base}/{}", dir.subdir)
    }
}

impl LockDeps for Deps<'_, '_, '_, '_> {
    type Tree = TreeHandle;

    fn get_flake(
        &mut self,
        flake_ref: &FlakeRef,
        use_registries: UseRegistries,
        lock_root_attr_path: &[String],
    ) -> brix_flake::Result<Flake<TreeHandle>> {
        // Inputs always need a lockable result: their hash goes into the
        // lock file.
        self.get_flake_full(flake_ref, use_registries, lock_root_attr_path, true)
            .map_err(|e| self.flake_err(e))
    }

    fn read_flake_at(
        &mut self,
        flake_ref: &FlakeRef,
        dir: &FlakeDir<TreeHandle>,
        lock_root_attr_path: &[String],
    ) -> brix_flake::Result<Flake<TreeHandle>> {
        self.read_flake(
            flake_ref.clone(),
            flake_ref.clone(),
            flake_ref.clone(),
            dir.clone(),
            lock_root_attr_path,
        )
        .map_err(|e| self.flake_err(e))
    }

    fn fetch_tree(
        &mut self,
        input: &Input,
        use_registries: UseRegistries,
    ) -> brix_flake::Result<FetchedInput<TreeHandle>> {
        let (tree, resolved, extra_attrs) = self
            .fetch_and_mount(input, use_registries, true)
            .map_err(|e| self.flake_err(e))?;
        Ok(FetchedInput {
            locked: tree.input.clone(),
            tree,
            resolved,
            extra_attrs,
        })
    }

    fn source_path(&mut self, input: &Input) -> brix_flake::Result<Option<String>> {
        brix_flake::native::source_path(input)?
            .map(|p| match p.to_str() {
                Some(s) => Ok(s.to_owned()),
                None => Err(brix_flake::Error::Message(format!(
                    "path '{}' is not UTF-8",
                    p.display()
                ))),
            })
            .transpose()
    }

    fn show_flake_path(&mut self, dir: &FlakeDir<TreeHandle>) -> String {
        let abs = format!("{}/flake.nix", flake_dir_path(self.host, dir));
        let canon = CanonPath::new(&abs)
            .expect("invariant: the flake.nix of a locked flake was read at this path");
        let cx = self.cx.borrow();
        let path = brix_eval::value::PathVal {
            accessor: cx.root_accessor(),
            path: canon.as_bytes(),
        };
        cx.services().show_path(&path).to_string()
    }

    /// `Logger::warn`: the message as it is after `warning: `, continuation
    /// lines unindented.
    fn warn(&mut self, msg: &str) {
        crate::log::stderr(&format!("warning: {msg}"));
    }

    /// Starts the announced fetches in the background, each resolved
    /// through the registries as [`Deps::fetch_and_mount`] will resolve
    /// it. One that cannot be resolved is left for the foreground to
    /// report.
    fn prefetch(&mut self, inputs: &[(Input, UseRegistries)]) {
        let resolved = inputs.iter().filter_map(|(input, use_registries)| {
            if input.is_direct() {
                return Some(input.clone());
            }
            let registries = self.host.registries();
            lookup_in_registries(&registries, input, *use_registries)
                .ok()
                .map(|(resolved, _)| resolved)
        });
        self.host.prefetch(resolved.collect::<Vec<_>>());
    }

    fn read_file(&mut self, tree: &TreeHandle, path: &str) -> brix_flake::Result<Option<String>> {
        let Some(accessor) = self.tree_accessor(&tree.root) else {
            return Err(brix_flake::Error::Message(format!(
                "tree '{}' is not mounted",
                tree.root.mount_path(self.host.store_dir())
            )));
        };
        // A relative input that climbs out of its tree is outside the
        // accessor; joining would silently read the tree's own file.
        if path == ".." || path.starts_with("../") {
            return Err(brix_flake::Error::Message(format!(
                "cannot read '{path}' relative to '{}': brix does not read flakes outside the tree a relative input is in",
                tree.input
            )));
        }
        // Relative to the tree's root.
        let canon = CanonPath::new(path).map_err(|e| brix_flake::Error::Message(e.to_string()))?;
        match accessor.maybe_lstat(&canon) {
            Ok(None) => Ok(None),
            Ok(Some(_)) => {
                let bytes = accessor
                    .read_file(&canon)
                    .map_err(|e| brix_flake::Error::Message(e.to_string()))?;
                String::from_utf8(bytes).map(Some).map_err(|_| {
                    brix_flake::Error::Message(format!("'{path}' in '{}' is not UTF-8", tree.input))
                })
            }
            Err(e) => Err(brix_flake::Error::Message(e.to_string())),
        }
    }
}

/// `flakeDir` of `parseFlakeInput`: the directory of the `flake.nix` being
/// read, as a path value, and as Nix shows it in messages.
struct FlakeDirPath {
    accessor: brix_store::AccessorId,
    path: CanonPath,
    shown: String,
}

/// What `readFlake` does to a value before it looks at its type.
#[derive(Clone, Copy)]
enum Forcing {
    /// `forceTrivialValue`: forced when that is trivial
    /// ([`Evaluator::is_trivial`]); any other thunk stays a thunk, which
    /// is then of the wrong type (`expected a string but got a thunk`).
    Trivial,
    /// Nothing: `parseFlakeInputAttr` takes the type the value has, a
    /// thunk if nothing evaluated it yet.
    Never,
}

/// The value in `cell` after `forcing`.
fn read<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    forcing: Forcing,
) -> brix_eval::Result<()> {
    if matches!(forcing, Forcing::Trivial) && cx.is_trivial(cell)? {
        cx.force_at(cell, SrcPos::NONE)?;
    }
    Ok(())
}

/// Read only the fields Nix's readFlake forces, in the same order.
fn flake_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    cx.force_at(cell, SrcPos::NONE)?;
    if !matches!(cx.view(cell)?, GcView::Attrs(_)) {
        return leaf_value(cx, cell, flake_dir);
    }
    const READ: [&str; 4] = ["description", "inputs", "outputs", "nixConfig"];
    let mut map = BTreeMap::new();
    let mut read_attrs: [Option<(SrcPos, GcCallValue<'h>)>; 4] = std::array::from_fn(|_| None);
    for (name, pos, value) in cx.attrs_with_pos(cell)? {
        let name = flake_attr_name_bytes(cx.services(), &name)?;
        match READ.iter().position(|&r| r == name) {
            Some(i) => read_attrs[i] = Some((pos, value)),
            None => {
                map.insert(
                    name,
                    flake_attr(
                        cx.services(),
                        FlakeValue::Other("an unsupported attribute".into()),
                        pos,
                    ),
                );
            }
        }
    }
    for (name, attr) in READ.into_iter().zip(read_attrs) {
        let Some((pos, cell)) = attr else {
            continue;
        };
        let value = match name {
            "inputs" => inputs_value(cx, &cell, flake_dir)?,
            "nixConfig" => nix_config_value(cx, &cell, flake_dir)?,
            _ => {
                read(cx, &cell, Forcing::Trivial)?;
                leaf_value(cx, &cell, flake_dir)?
            }
        };
        map.insert(name.to_owned(), flake_attr(cx.services(), value, pos));
    }
    Ok(FlakeValue::Attrs(map))
}

fn inputs_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    attrs_value(cx, cell, flake_dir, |cx, name, value, _| match name {
        "self" => attrs_value(cx, value, flake_dir, |cx, name, value, pos| {
            input_attr_value(cx, name, value, pos, flake_dir)
        }),
        _ => input_value(cx, value, flake_dir),
    })
}

fn input_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    attrs_value(cx, cell, flake_dir, |cx, name, value, pos| {
        let result = (|| match name {
            "inputs" => inputs_value(cx, value, flake_dir),
            "url" => {
                read(cx, value, Forcing::Trivial)?;
                if let GcView::Path { accessor, bytes } = cx.view(value)? {
                    let path = brix_eval::value::PathVal {
                        accessor,
                        path: bytes,
                    };
                    if cx.path_literal_anchor(value, pos)? != Some(PathAnchor::Source) {
                        foreign_path(cx.services(), &path, flake_dir)
                    } else {
                        leaf_value(cx, value, flake_dir)
                    }
                } else {
                    leaf_value(cx, value, flake_dir)
                }
            }
            "flake" | "buildTime" | "follows" => {
                read(cx, value, Forcing::Trivial)?;
                leaf_value(cx, value, flake_dir)
            }
            _ => input_attr_value(cx, name, value, pos, flake_dir),
        })();
        result.inspect_err(|error| {
            cx.services().add_trace(
                error,
                pos,
                format!("while evaluating flake attribute '{name}'"),
            )
        })
    })
}

fn input_attr_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    name: &str,
    cell: &GcCallValue<'h>,
    pos: SrcPos,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    read(cx, cell, Forcing::Never)?;
    let scalar = matches!(
        cx.view(cell)?,
        GcView::String(_) | GcView::Bool(_) | GcView::Int(_)
    );
    if name != "publicKeys" || scalar || !cx.services().experimental("verified-fetches") {
        return leaf_value(cx, cell, flake_dir);
    }
    let bytes = cx.to_json_with_options(cell, pos, true)?;
    Ok(FlakeValue::String(
        String::from_utf8(bytes).expect("JSON is UTF-8"),
    ))
}

fn nix_config_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    attrs_value(cx, cell, flake_dir, |cx, _, value, _| {
        read(cx, value, Forcing::Trivial)?;
        if let GcView::Path { accessor, bytes } = cx.view(value)? {
            let path = brix_eval::value::PathVal {
                accessor,
                path: bytes,
            };
            cx.services().check_path_access(&path)?;
            let canon = CanonPath::new(bytes)
                .map_err(|e| cx.services().error(ErrorKind::Eval, e.message_bytes()))?;
            let exists = cx
                .services()
                .accessor(accessor)
                .maybe_lstat(&canon)
                .map_err(|e| cx.services().error(ErrorKind::Eval, e.message_bytes()))?
                .is_some();
            if !exists {
                return Err(cx.services().error(
                    ErrorKind::Eval,
                    [
                        &b"path '"[..],
                        &cx.services().show_path(&path),
                        b"' does not exist",
                    ]
                    .concat(),
                ));
            }
        }
        leaf_value(cx, value, flake_dir)
    })
}

fn attrs_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
    attr: impl Fn(
        &mut GcCallContext<'_, '_, 'h>,
        &str,
        &GcCallValue<'h>,
        SrcPos,
    ) -> brix_eval::Result<FlakeValue>,
) -> brix_eval::Result<FlakeValue> {
    read(cx, cell, Forcing::Trivial)?;
    if !matches!(cx.view(cell)?, GcView::Attrs(_)) {
        return leaf_value(cx, cell, flake_dir);
    }
    let mut map = BTreeMap::new();
    for (name, pos, value) in cx.attrs_with_pos(cell)? {
        let name = flake_attr_name_bytes(cx.services(), &name)?;
        let value = attr(cx, &name, &value, pos)?;
        map.insert(name, flake_attr(cx.services(), value, pos));
    }
    Ok(FlakeValue::Attrs(map))
}

fn leaf_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    cell: &GcCallValue<'h>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    Ok(match cx.view(cell)? {
        GcView::String(_) => {
            let bytes = cx.string_bytes(cell)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                cx.services().error(
                    ErrorKind::Eval,
                    [
                        &b"string '"[..],
                        &bytes,
                        b"' in a flake is not valid UTF-8, which brix does not support",
                    ]
                    .concat(),
                )
            })?;
            FlakeValue::String(text.to_owned())
        }
        GcView::Int(n) => FlakeValue::Int(n),
        GcView::Bool(b) => FlakeValue::Bool(b),
        GcView::Path { accessor, bytes } => {
            let path = brix_eval::value::PathVal {
                accessor,
                path: bytes,
            };
            if accessor != flake_dir.accessor {
                return foreign_path(cx.services(), &path, flake_dir);
            }
            let canon = CanonPath::new(bytes)
                .map_err(|e| cx.services().error(ErrorKind::Eval, e.message_bytes()))?;
            let relative = flake_dir.path.make_relative(&canon);
            FlakeValue::Path(String::from_utf8(relative.into()).map_err(|_| {
                cx.services().error(
                    ErrorKind::Eval,
                    non_utf8_path_message(&cx.services().show_path(&path)),
                )
            })?)
        }
        GcView::List(list) => {
            let children: Vec<_> = (0..list.len()).map(|i| list.get(i).unwrap()).collect();
            FlakeValue::List(
                children
                    .iter()
                    .map(|value| leaf_value(cx, value, flake_dir))
                    .collect::<brix_eval::Result<_>>()?,
            )
        }
        GcView::Attrs(_) => FlakeValue::Attrs(BTreeMap::new()),
        GcView::Function => FlakeValue::Function {
            formals: cx.function_formals(cell)?.map(|names| {
                names
                    .into_iter()
                    .map(|name| String::from_utf8(name).expect("formal identifiers are UTF-8"))
                    .collect()
            }),
        },
        _ => FlakeValue::Other(cx.show_type(cell)?),
    })
}

fn flake_attr_name_bytes(ev: &Evaluator<'_>, bytes: &[u8]) -> brix_eval::Result<String> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
        ev.error(
            ErrorKind::Eval,
            [
                &b"attribute name '"[..],
                bytes,
                b"' in a flake is not valid UTF-8, which brix does not support",
            ]
            .concat(),
        )
    })
}

/// An attribute name of `flake.nix` as text. A flake's inputs and
/// settings are named by text in brix (and in the lock file, JSON, which
/// Nix fails to write with other bytes too), so a name that is not UTF-8
/// is refused rather than altered.
/// brix's message for a path of `flake.nix` whose name is not UTF-8
/// (`shown` as Nix shows it): brix-flake takes a flake's paths as text, as
/// the lock file they end up in is (JSON, which Nix fails to write with
/// such a name too), so it is refused rather than altered.
fn non_utf8_path_message(shown: &[u8]) -> Vec<u8> {
    let mut msg = b"path '".to_vec();
    msg.extend_from_slice(shown);
    msg.extend_from_slice(b"' in a flake is not valid UTF-8, which brix does not support");
    msg
}

/// A path of another tree than the flake's (`parseFlakeInput` refuses it
/// as `url`, `readFlake` as anything else), as brix-flake reports it: how
/// Nix shows the path and the flake's directory, as text.
fn foreign_path(
    ev: &Evaluator<'_>,
    p: &brix_eval::value::PathVal<'_>,
    flake_dir: &FlakeDirPath,
) -> brix_eval::Result<FlakeValue> {
    let shown = ev.show_path(p);
    let path = String::from_utf8(shown.into())
        .map_err(|e| ev.error(ErrorKind::Eval, non_utf8_path_message(e.as_bytes())))?;
    Ok(FlakeValue::ForeignPath {
        path,
        flake_dir: flake_dir.shown.clone(),
    })
}

/// An attribute with its position: as Nix prints a `PosIdx` in a
/// message, and the position itself, for [`read_flake_error`] to show.
/// brix-flake's messages are text, so a file name that is not UTF-8 is
/// shown with U+FFFD there (`flake.nix` is in a flake's directory, which
/// is text: see `read_flake`).
fn flake_attr(ev: &Evaluator<'_>, value: FlakeValue, pos: SrcPos) -> brix_flake::FlakeAttr {
    let shown = ev
        .position(pos)
        .map_or_else(|| "«none»:0".to_owned(), |p| p.display().to_string());
    brix_flake::FlakeAttr {
        value,
        pos: FlakePos {
            shown,
            handle: pos.0,
        },
    }
}

/// `readFlake`'s error as Nix shows it: its traces (`while evaluating
/// flake attribute '…'`, `while evaluating flake input`) with the code at
/// the positions in `flake.nix` they name, as an evaluation error's are
/// shown, and cut short without `--show-trace` as those are. It is the
/// flake's evaluation error from then on, so the lock algorithm's traces
/// and `getFlake`'s go on it as on any other.
fn read_flake_error(ev: &Evaluator<'_>, e: &brix_flake::Error) -> brix_eval::Error {
    let err = ev.error(ErrorKind::Eval, e.message());
    // The evaluator keeps traces innermost first.
    for (pos, trace) in e.traces_at().into_iter().rev() {
        err.add_trace(pos.and_then(|p| ev.position(SrcPos(p.handle))), trace);
    }
    err
}

/// Why [`lock_and_call_raw`] failed.
pub(crate) enum LockAndCallError {
    /// Evaluating a `flake.nix` or `call-flake.nix` failed: the error
    /// itself, for `builtins.getFlake` to rethrow with its traces.
    Eval(brix_eval::Error),
    /// Anything else (fetching, locking).
    Other(Error),
}

impl LockAndCallError {
    /// The error as the facade reports it.
    fn into_error(self, ev: &Evaluator<'_>, host: &NativeHost) -> Error {
        match self {
            LockAndCallError::Eval(e) => host.eval_error(&e, ev.config.show_trace),
            LockAndCallError::Other(e) => e,
        }
    }
}

/// Locks `flake_ref` and returns its lock and its value. `require_lockable`
/// is Nix's `LockFlags::requireLockable`: whether the root flake's own
/// reference must end up lockable (`nix flake metadata` says no).
pub(crate) fn lock_and_call<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    flake_ref: &FlakeRef,
    flags: &LockFlags,
    require_lockable: bool,
) -> Result<(LockedFlake<TreeHandle>, GcCallValue<'h>)> {
    lock_and_call_raw(cx, host, flake_ref, flags, require_lockable)
        .map_err(|e| e.into_error(cx.services(), host))
}

/// [`lock_and_call`] with evaluation errors kept whole.
pub(crate) fn lock_and_call_raw<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    flake_ref: &FlakeRef,
    flags: &LockFlags,
    require_lockable: bool,
) -> std::result::Result<(LockedFlake<TreeHandle>, GcCallValue<'h>), LockAndCallError> {
    let locked = lock_raw(cx, host, flake_ref, flags, require_lockable)?;
    let value = crate::callflake::call_flake(cx, host, &locked).map_err(LockAndCallError::Eval)?;
    Ok((locked, value))
}

/// `lockFlake` without `callFlake`: what `nix flake metadata`, `lock`,
/// `update` and `prefetch-inputs` do. Calling the flake can fail where
/// locking it does not: a relative input that leaves its tree for the
/// store directory itself (`path:..` at the root of a tree) is locked,
/// but `callFlake` cannot take its store path.
pub(crate) fn lock(
    cx: &mut GcCallContext<'_, '_, '_>,
    host: &NativeHost,
    flake_ref: &FlakeRef,
    flags: &LockFlags,
    require_lockable: bool,
) -> Result<LockedFlake<TreeHandle>> {
    lock_raw(cx, host, flake_ref, flags, require_lockable)
        .map_err(|e| e.into_error(cx.services(), host))
}

/// [`lock`] with evaluation errors kept whole: the flake fetched and
/// read, locked, and the lock file written where the lock algorithm
/// decided.
fn lock_raw<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    flake_ref: &FlakeRef,
    flags: &LockFlags,
    require_lockable: bool,
) -> std::result::Result<LockedFlake<TreeHandle>, LockAndCallError> {
    let mut deps = Deps::new(cx, host);
    let use_registries = if flags.use_registries {
        UseRegistries::All
    } else {
        UseRegistries::No
    };
    // The evaluation error behind a failure, if one is, is in `deps`.
    let failed = |deps: &Deps<'_, '_, '_, '_>, e: Error| match deps.eval_error.borrow_mut().take() {
        Some(inner) => LockAndCallError::Eval(inner),
        None => LockAndCallError::Other(e),
    };
    let flake = deps
        .get_flake_full(flake_ref, use_registries, &[], require_lockable)
        .map_err(|e| failed(&deps, e))?;
    let locked =
        brix_flake::lock_flake(&host.settings, &mut deps, flake, flags).map_err(|e| match deps
            .eval_error
            .borrow_mut()
            .take()
        {
            Some(inner) => {
                // The lock algorithm's traces go on the evaluation error,
                // outside its own, as Nix's `addTrace` puts them.
                for trace in e.traces().into_iter().rev() {
                    inner.add_trace(None, trace);
                }
                LockAndCallError::Eval(inner)
            }
            None => LockAndCallError::Other(Error::Flake(e)),
        })?;
    // Nix reads the flake again after writing its lock file. brix's trees
    // already show what that gives: a `path:` tree is read in place, so
    // the new file is in it, and a Git working tree keeps the files it was
    // fetched with, as Nix's cached `WorkdirInfo` does in the same process.
    write_lock_file(flake_ref, &locked).map_err(LockAndCallError::Other)?;
    Ok(locked)
}

/// The end of `lockFlake`: the new lock file written where the lock
/// algorithm decided ([`LockWrite`]), the flake's own announced first.
fn write_lock_file(top: &FlakeRef, locked: &LockedFlake<TreeHandle>) -> Result<()> {
    match &locked.write {
        LockWrite::Nothing => Ok(()),
        LockWrite::Output { path, contents } => write_file(path, contents),
        LockWrite::Source { path, contents } => {
            let existed = std::path::Path::new(path).exists();
            // `warn`, as the lock algorithm's own warnings.
            crate::log::stderr(&format!(
                "warning: {}",
                LockWrite::announcement(path, existed, &locked.diff)
            ));
            let rel = if top.subdir.is_empty() {
                "flake.lock".to_owned()
            } else {
                format!("{}/flake.lock", top.subdir)
            };
            put_file(&top.input, path, &rel, contents)
        }
    }
}

/// `writeFile`.
fn write_file(path: &str, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| Error::io(format!("opening file \"{path}\""), e))
}

/// `InputScheme::putFile` of the file `rel` of the source of `input`,
/// which is at `path`: written and, in a Git working tree, added with
/// `git add --intent-to-add` unless Git ignores it
/// (`GitInputScheme::putFile`), in a Mercurial one with `hg add`
/// (`MercurialInputScheme::putFile`), so that the next fetch of the tree
/// sees it.
fn put_file(input: &Input, path: &str, rel: &str, contents: &str) -> Result<()> {
    write_file(path, contents)?;
    if input.scheme == Some(brix_flake::Scheme::Mercurial) {
        // Only a working tree has a source path to write to.
        brix_flake::native::mercurial::add_file(std::path::Path::new(path))?;
    } else if brix_flake::native::git::is_local_checkout(input)? {
        let repo = path
            .strip_suffix(rel)
            .and_then(|p| p.strip_suffix('/'))
            .expect("the lock file is the file it names in its repository");
        let git = |args: &[&str]| -> Result<std::process::ExitStatus> {
            std::process::Command::new("git")
                .args(["-C", repo, "--git-dir", ".git"])
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .map_err(|e| Error::io("executing 'git'", e))
        };
        if !git(&["check-ignore", "--quiet", rel])?.success() {
            let status = git(&["add", "--intent-to-add", "--", rel])?;
            if !status.success() {
                return Err(Error::Config(format!(
                    "program 'git' failed with exit code {}",
                    status
                        .code()
                        .map_or_else(|| "unknown".into(), |c| c.to_string())
                )));
            }
        }
    }
    Ok(())
}

/// `LockedFlake::getFingerprint` for a flake this session locked (see
/// [`brix_flake::fingerprint`]), with the root input's fingerprint as
/// the tree it was fetched as gives it: a Git working tree's from its
/// changed files, a `path:` store path's from the store's NAR hash (none
/// without a store, as in Nix), the rest from the input and the tree.
pub(crate) fn fingerprint(
    host: &NativeHost,
    locked: &LockedFlake<TreeHandle>,
) -> Result<Option<brix_store::Hash>> {
    let flake = &locked.flake;
    let requested = &flake.resolved_ref.input;
    let input = &flake.locked_ref.input;
    let accessor = match &flake.dir.tree.root {
        TreeRoot::Store(sp) => host.tree(sp).map(|t| t.accessor),
        TreeRoot::Lazy { id, .. } => host.lazy_tree(*id).map(|t| t.accessor),
    }
    .ok_or_else(|| {
        Error::Store(
            format!(
                "tree '{}' is not mounted",
                flake.dir.tree.root.mount_path(host.store_dir())
            )
            .into(),
        )
    })?;
    let workdir = brix_flake::native::git::local_workdir(requested)?;
    let input_fp = match (input.type_name()?, &workdir) {
        ("git", Some(path)) if input.rev()?.is_none() => {
            let workdir = brix_flake::native::git_workdir::Workdir::open(path)?;
            brix_flake::native::git_workdir::dirty_fingerprint(&workdir, input)?
        }
        ("path", _) => match input.attrs.get("path") {
            Some(brix_flake::Attr::String(p)) => match host.store_dir().maybe_parse(p) {
                Some(sp) if host.store_dir().display(&sp) == *p => host
                    .daemon_nar_hash(&sp)?
                    .map(|h| format!("path:{}", h.to_string(brix_store::HashFormat::Sri, true))),
                _ => None,
            },
            _ => None,
        },
        _ => brix_flake::fingerprint::input_fingerprint(input, &*accessor)?,
    };
    let computed =
        brix_flake::fingerprint::rev_count_computed(requested, input, workdir.is_some())?;
    Ok(brix_flake::fingerprint::flake_fingerprint(
        &host.settings,
        locked,
        input_fp.as_deref(),
        computed,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_dir_wins_over_the_references() {
        let settings = brix_flake::FetchSettings::default();
        let opts = brix_flake::ParseOptions::flake();
        let with_dir = brix_flake::parse_flake_ref(&settings, "flake:x?dir=mine", &opts).unwrap();
        let without = brix_flake::parse_flake_ref(&settings, "flake:x", &opts).unwrap();
        let extra = brix_flake::attrs::attrs([("dir", "theirs".into())]);
        assert_eq!(flake_subdir(&extra, &with_dir).unwrap(), "theirs");
        assert_eq!(flake_subdir(&extra, &without).unwrap(), "theirs");
        assert_eq!(flake_subdir(&Attrs::new(), &with_dir).unwrap(), "mine");
        let bad = brix_flake::attrs::attrs([("dir", 1u64.into())]);
        assert!(flake_subdir(&bad, &with_dir).is_err());
    }
    #[test]
    fn managed_flake_callbacks_and_lazy_source_survive_every_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let child_source = b"{ outputs = { self }: { answer = 7; }; }";
        std::fs::write(child.join("flake.nix"), child_source).unwrap();
        let mut files = brix_store::MemoryAccessor::new();
        files
            .add_file(&CanonPath::new("/flake.nix").unwrap(), child_source, false)
            .unwrap();
        let (child_hash, _) = brix_store::nar::hash_path(
            &files,
            &CanonPath::new("/").unwrap(),
            brix_store::FileIngestionMethod::NixArchive,
            brix_store::HashAlgo::Sha256,
            &mut |_| Ok(true),
        )
        .unwrap();
        let child_ref = format!(
            "path:{}?narHash={}",
            child.display(),
            child_hash.to_string(brix_store::HashFormat::Sri, true)
        );
        std::fs::write(temp.path().join("flake.nix"), r#"{
            description = "managed";
            outputs = { self }: {
                answer = builtins.foldl' builtins.add 0 [ 20 22 ];
                roundTrip = builtins.flakeRefToString (builtins.parseFlakeRef "github:NixOS/nixpkgs/23.05?dir=lib");
                file = builtins.readFile ./message;
                nested = (builtins.getFlake "CHILD_REF").answer;
            };
        }"#.replace("CHILD_REF", &child_ref)).unwrap();
        std::fs::write(temp.path().join("message"), "hello").unwrap();
        let mut config = crate::Config::default();
        config.gc.stress = true;
        config.gc.chunk_bytes = 256;
        config.gc.threshold_bytes = 1024;
        let cache = tempfile::tempdir().unwrap();
        config.fetch.cache_dir = Some(cache.path().to_owned());
        config.fetch.flake_registry.clear();
        let session = crate::Session::new(config).unwrap();
        let reference = session
            .parse_flake_ref(&format!("path:{}", temp.path().display()), "/")
            .unwrap();
        let flake = session
            .flake_with_flags(&reference, &LockFlags::pure(), false)
            .unwrap();
        let output = flake.outputs().unwrap();
        assert_eq!(output.get_attr("answer").unwrap().get_int().unwrap(), 42);
        assert_eq!(output.get_attr("nested").unwrap().get_int().unwrap(), 7);
        assert_eq!(
            output.get_attr("roundTrip").unwrap().get_string().unwrap(),
            "github:NixOS/nixpkgs/23.05?dir=lib"
        );
        assert_eq!(
            output.get_attr("file").unwrap().get_string().unwrap(),
            "hello"
        );
        session.collect_garbage().unwrap();
        assert_eq!(output.get_attr("answer").unwrap().get_int().unwrap(), 42);
        assert_eq!(output.get_attr("nested").unwrap().get_int().unwrap(), 7);
        assert!(session.gc_stats().collections > 0);
        let source = flake.root().get_attr("sourceInfo").unwrap();
        assert!(source
            .get_attr("outPath")
            .unwrap()
            .get_string()
            .unwrap()
            .starts_with("/nix/store/"));
        assert!(source
            .get_attr("narHash")
            .unwrap()
            .get_string()
            .unwrap()
            .starts_with("sha256-"));
    }
}
