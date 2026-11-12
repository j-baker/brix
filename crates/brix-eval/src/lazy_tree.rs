//! Lazy trees: a fetched tree that is mounted before its store path is
//! known.
//!
//! A tree's store path is `makeFixedOutputPath(name, NAR hash)`, so naming
//! it means reading and hashing every file in it. The top-level flake of a
//! local checkout (a `path:` directory, a Git working tree with changes)
//! has no hash to start from, and most evaluations never need its store
//! path: they read files from it and copy parts of it (`src = ./app`),
//! which are hashed on their own.
//!
//! Such a tree is mounted at a placeholder in the store directory,
//! `<store>/.brix-lazy-<n>-<name>`. That is not a store path (none starts
//! with a dot), and because it sits in the store directory, `..` and
//! `dirOf` lead where they would from the real store path. Paths into the
//! tree are ordinary path values on the root accessor, so reading,
//! importing and copying parts of the tree work unchanged.
//!
//! A path into the tree that becomes text (`toString ./.`, a position's
//! file name, a path filter's argument) is `<store>/<hash>-<name>/…` with
//! the hash left lazy: placeholder bytes, and a hidden record of where
//! they are and whose hash they stand for (see the second half of this
//! file). The operations that need only what is known without the hash
//! keep it lazy; every other way of reading a string computes it first,
//! which materialises the tree: hashes it, mounts it at its real store
//! path too, and records that path. The tree's `outPath` and `narHash`
//! are thunks that materialise it. Results are therefore exactly those of
//! hashing up front, only computed later, or never.

use crate::runtime::cell::OnceCell;
use crate::runtime::rc::Rc;
use std::borrow::Cow;
use std::sync::Arc;

use brix_store::{
    CanonPath, ContentAddressMethod, Hash, HashFormat, SourceAccessor, SourcePath, StoreObject,
    StorePath,
};
use bstr::BString;

use crate::coerce::Coercion;
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::value::{ContextElem, NixStr, PathVal, PrimOp, SrcPos, Value, ValueRef};
use crate::Result;

/// A lazily mounted tree, by registration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LazyTreeId(pub u32);

pub(crate) struct LazyTree {
    /// `<store>/.brix-lazy-<n>-<name>`.
    placeholder: CanonPath,
    /// The store path name (`source`).
    name: String,
    /// What is mounted at the placeholder; mounted at the store path too
    /// once that is known, so the tree shows the same either way.
    mounted: Arc<dyn SourceAccessor>,
    store_path: OnceCell<StorePath>,
    materializing: crate::runtime::Gate,
}

impl<'a> Evaluator<'a> {
    /// Mounts `accessor` as a lazy tree with store path name `name`, its
    /// files shown under `display_prefix` (the checkout's directory, or
    /// `«locked url»`). Returns the tree and the placeholder it is
    /// readable at.
    pub fn mount_lazy_tree(
        &self,
        accessor: Arc<dyn SourceAccessor>,
        name: &str,
        display_prefix: impl Into<BString>,
    ) -> (LazyTreeId, CanonPath) {
        let mut trees = self.lazy_trees.borrow_mut();
        let id = LazyTreeId(trees.len() as u32);
        let placeholder = CanonPath::new(format!("{}{}-{}", self.lazy_prefix(), id.0, name))
            .expect("a store directory and a store path name make a canonical path");
        let mounted: Arc<dyn SourceAccessor> = Arc::new(crate::mount::DisplayPrefixAccessor::new(
            accessor,
            display_prefix,
        ));
        self.mount(placeholder.clone(), mounted.clone());
        self.allow_path(placeholder.clone());
        trees.push(Rc::new(LazyTree {
            placeholder: placeholder.clone(),
            name: name.to_owned(),
            mounted,
            store_path: OnceCell::new(),
            materializing: Default::default(),
        }));
        (id, placeholder)
    }

    /// The tree's store path, hashing the tree the first time.
    pub fn materialize(&self, id: LazyTreeId) -> Result<StorePath> {
        let tree = self.lazy_trees.borrow()[id.0 as usize].clone();
        if let Some(sp) = tree.store_path.get() {
            return Ok(sp.clone());
        }
        let _materializing = tree.materializing.lock();
        #[cfg(brix_parallel)]
        if let Some(sp) = tree.store_path.get() {
            return Ok(sp.clone());
        }
        // Fetching the tree into the store under its own name, as Nix
        // does before evaluation without lazy trees. Copying a whole lazy
        // tree records its store path (`note_lazy_root_hashed`).
        let source = SourcePath::new(self.root_accessor, tree.placeholder.clone());
        self.add_path_to_store(
            &source,
            &tree.name,
            ContentAddressMethod::NixArchive,
            None,
            SrcPos::NONE,
        )?;
        Ok(tree
            .store_path
            .get()
            .expect("an unfiltered copy of a lazy tree's root records the tree's store path")
            .clone())
    }

    /// Called after a whole tree was hashed for an unfiltered copy
    /// (`builtins.path { path = ./.; }`, `"${./.}"`, [`Evaluator::materialize`]).
    /// When `path` is a lazy tree's root, its NAR hash is the tree's, so
    /// the tree's store path is known for free: the first time, the tree
    /// is mounted there, shown as the tree itself (wherever the path is
    /// learnt from), and the path is recorded. Returns the tree's store
    /// path, `None` when `path` is not a lazy tree's root.
    pub(crate) fn note_lazy_root_hashed(
        &self,
        path: &[u8],
        nar_hash: &Hash,
    ) -> Result<Option<StorePath>> {
        let Some(id) = self.lazy_root(path) else {
            return Ok(None);
        };
        let tree = self.lazy_trees.borrow()[id.0 as usize].clone();
        if let Some(sp) = tree.store_path.get() {
            return Ok(Some(sp.clone()));
        }
        let _materializing = tree.materializing.lock();
        #[cfg(brix_parallel)]
        if let Some(sp) = tree.store_path.get() {
            return Ok(Some(sp.clone()));
        }
        let source = SourcePath::new(self.root_accessor, tree.placeholder.clone());
        let sp = self
            .store
            .borrow_mut()
            .add_known_tree(&source, &tree.name, *nar_hash)
            .map_err(|e| self.error(crate::ErrorKind::Eval, e.to_string()))?;
        let at = self.store_path_mount_point(&sp);
        self.mount(at.clone(), tree.mounted.clone());
        self.allow_path(at);
        tree.store_path
            .set(sp.clone())
            .expect("the store path was unset above, and nothing since evaluates");
        Ok(Some(sp))
    }

    /// The tree's `narHash` in SRI form, hashing the tree the first time.
    pub fn lazy_tree_nar_hash(&self, id: LazyTreeId) -> Result<Hash> {
        let sp = self.materialize(id)?;
        match self.store.borrow().get(&sp) {
            Some(StoreObject::SourceTree { hash, .. }) => Ok(*hash),
            _ => unreachable!("a materialised lazy tree is a source tree in the virtual store"),
        }
    }

    /// A thunk for the tree's `outPath`: its store path with context.
    pub fn lazy_tree_out_path_thunk(&self, id: LazyTreeId) -> ValueRef<'a> {
        self.lazy_tree_thunk("lazyTreeOutPath", id, |ev, id| {
            let sp = ev.materialize(id)?;
            Ok(ev.store_path_string(&sp))
        })
    }

    /// A thunk for the tree's `narHash`.
    pub fn lazy_tree_nar_hash_thunk(&self, id: LazyTreeId) -> ValueRef<'a> {
        self.lazy_tree_thunk("lazyTreeNarHash", id, |ev, id| {
            let h = ev.lazy_tree_nar_hash(id)?;
            Ok(ev.mk_string(&h.to_string(HashFormat::Sri, true)))
        })
    }

    fn lazy_tree_thunk(
        &self,
        name: &'static str,
        id: LazyTreeId,
        f: fn(&Evaluator<'a>, LazyTreeId) -> Result<Value<'a>>,
    ) -> ValueRef<'a> {
        // Not registered as a global: nothing new appears in `builtins`.
        let imp: &'a crate::value::PrimOpFn<'a> = self.bump.alloc(
            move |ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]| {
                let n = ev.force_int(args[0], pos, "while evaluating a lazy tree id")?;
                f(ev, LazyTreeId(n as u32))
            },
        );
        let fun = self.alloc_primop(PrimOp {
            name,
            arity: 1,
            add_trace: false,
            imp,
        });
        self.mk_app(fun, self.alloc_cell(Value::Int(i64::from(id.0))))
    }

    /// `<store>/.brix-lazy-`: what every placeholder starts with.
    fn lazy_prefix(&self) -> String {
        format!("{}/.brix-lazy-", self.config.store_dir.as_str())
    }

    /// The lazy tree `path` (absolute, on the root accessor) lies in, with
    /// the rest of the path after the placeholder.
    fn find_lazy<'s>(&self, path: &'s [u8]) -> Option<(Rc<LazyTree>, &'s [u8])> {
        let trees = self.lazy_trees.borrow();
        if trees.is_empty() || !path.starts_with(self.lazy_prefix().as_bytes()) {
            return None;
        }
        trees.iter().find_map(|t| {
            let rest = path.strip_prefix(t.placeholder.as_bytes())?;
            (rest.is_empty() || rest[0] == b'/').then(|| (t.clone(), rest))
        })
    }

    /// `path` (absolute, on the root accessor) as text: a path in a lazy
    /// tree is rewritten to the tree's store path, materialising it.
    pub fn logical_path<'s>(&self, path: &'s [u8]) -> Result<Cow<'s, [u8]>> {
        let Some((tree, rest)) = self.find_lazy(path) else {
            return Ok(Cow::Borrowed(path));
        };
        let id = self.lazy_tree_id(&tree);
        let sp = self.materialize(id)?;
        let mut text = self.config.store_dir.display(&sp).into_bytes();
        text.extend_from_slice(rest);
        Ok(Cow::Owned(text))
    }

    fn lazy_tree_id(&self, tree: &Rc<LazyTree>) -> LazyTreeId {
        let trees = self.lazy_trees.borrow();
        let i = trees
            .iter()
            .position(|t| Rc::ptr_eq(t, tree))
            .expect("a found lazy tree is registered");
        LazyTreeId(i as u32)
    }

    /// The text of a path value: [`Evaluator::logical_path`] on the root
    /// accessor, the path itself elsewhere.
    pub fn path_text<'p>(&self, p: &PathVal<'p>) -> Result<Cow<'p, [u8]>> {
        if p.accessor == self.root_accessor {
            self.logical_path(p.path)
        } else {
            Ok(Cow::Borrowed(p.path))
        }
    }

    /// The lazy tree whose root `path` is.
    pub(crate) fn lazy_root(&self, path: &[u8]) -> Option<LazyTreeId> {
        let (tree, rest) = self.find_lazy(path)?;
        rest.is_empty().then(|| self.lazy_tree_id(&tree))
    }

    /// Whether two paths on the root accessor are equal. Paths in the same
    /// lazy tree (or in none) compare as bytes; otherwise the lazy side is
    /// materialised, unless the other side cannot be a store path at all.
    pub(crate) fn root_paths_equal(&self, x: &[u8], y: &[u8]) -> Result<bool> {
        if x == y {
            return Ok(true);
        }
        match (self.find_lazy(x), self.find_lazy(y)) {
            (None, None) => Ok(false),
            (Some((a, _)), Some((b, _))) if Rc::ptr_eq(&a, &b) => Ok(false),
            (lx, ly) => {
                let store = &self.config.store_dir;
                if (lx.is_none() && !store.is_in_store(x))
                    || (ly.is_none() && !store.is_in_store(y))
                {
                    return Ok(false);
                }
                Ok(self.logical_path(x)? == self.logical_path(y)?)
            }
        }
    }

    /// `x < y` for paths on the root accessor, as their texts compare:
    /// byte by byte, unsigned (`strcmp`).
    pub(crate) fn root_path_less(&self, x: &[u8], y: &[u8]) -> Result<bool> {
        match (self.find_lazy(x), self.find_lazy(y)) {
            (None, None) => Ok(x < y),
            (Some((a, _)), Some((b, _))) if Rc::ptr_eq(&a, &b) => Ok(x < y),
            _ => Ok(self.logical_path(x)? < self.logical_path(y)?),
        }
    }
}

// ----- lazy hashes in strings -----
//
// A path into a lazy tree that becomes text (`toString ./.`, a position's
// file name, a path filter's argument) need not compute the tree's hash:
// the text is `<store>/<hash>-<name>/…` with the hash left as a
// placeholder, and its place recorded in the string's context as a hidden
// `LazyHash` element (plus `LazyPath` for the `Path` context Nix would
// give it). What is known without hashing is exactly what the operations
// below use: the hash's length, the characters it cannot contain, and
// that two strings agree where they hold the same tree's hash at the same
// place. Everything else reads strings through `str_bytes`/`str_parts`,
// which compute the hash first, so an operation that does not know about
// lazy hashes is slower, never wrong.

/// The length of a store path hash.
pub(crate) const HASH_LEN: usize = 32;

/// What a lazy hash's bytes hold: a letter Nix's base-32 alphabet lacks,
/// so a placeholder can never be taken for a real hash.
const FILLER: u8 = b'e';

/// Whether `b` can occur in a store path hash.
fn in_hash_alphabet(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'z') && !matches!(b, b'e' | b'o' | b'u' | b't')
}

/// A lazy hash's place in a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LazyHash {
    pub offset: usize,
    pub tree: u32,
}

/// The lazy hashes of a string, in order (they sort first in its context).
pub(crate) fn lazy_hashes<'a>(s: &NixStr<'a>) -> impl Iterator<Item = LazyHash> + 'a {
    s.raw_context().iter().map_while(|c| match *c {
        ContextElem::LazyHash { offset, tree } => Some(LazyHash {
            offset: offset as usize,
            tree,
        }),
        _ => None,
    })
}

/// A string's context without its lazy hashes (still sorted).
pub(crate) fn context_without_hashes<'a>(s: &NixStr<'a>) -> &'a [ContextElem<'a>] {
    let ctx = s.raw_context();
    let n = ctx
        .iter()
        .take_while(|c| matches!(c, ContextElem::LazyHash { .. }))
        .count();
    &ctx[n..]
}

/// A string under construction whose parts may hold lazy hashes.
pub(crate) struct LazyStrBuilder<'a> {
    bytes: Vec<u8>,
    hashes: Vec<LazyHash>,
    /// The context so far, never holding a `LazyHash` (their offsets are
    /// kept in `hashes` until the string is finished).
    pub context: StringContext<'a>,
}

impl<'a> LazyStrBuilder<'a> {
    pub fn new() -> LazyStrBuilder<'a> {
        LazyStrBuilder {
            bytes: Vec::new(),
            hashes: Vec::new(),
            context: StringContext::new(),
        }
    }

    pub fn push_bytes(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }

    /// Appends a string, keeping its lazy hashes lazy.
    pub fn push_str(&mut self, s: &'a NixStr<'a>) {
        let base = self.bytes.len();
        self.hashes.extend(lazy_hashes(s).map(|h| LazyHash {
            offset: base + h.offset,
            tree: h.tree,
        }));
        self.bytes.extend_from_slice(s.raw_bytes());
        self.context.extend_from_slice(context_without_hashes(s));
    }

    /// Appends text holding one lazy hash at `hash_at`, with the `Path`
    /// context of that tree.
    fn push_lazy_text(&mut self, text: &[u8], hash_at: usize, tree: u32, with_context: bool) {
        self.hashes.push(LazyHash {
            offset: self.bytes.len() + hash_at,
            tree,
        });
        self.bytes.extend_from_slice(text);
        if with_context {
            self.context.insert(ContextElem::LazyPath(tree));
        }
    }

    pub fn finish(self, ev: &Evaluator<'a>) -> Value<'a> {
        if self.hashes.is_empty() {
            return ev.mk_string_arena(ev.alloc_str(&self.bytes), &self.context);
        }
        let mut context = self.context;
        for h in &self.hashes {
            context.insert(ContextElem::LazyHash {
                offset: h.offset as u32,
                tree: h.tree,
            });
        }
        Value::String(ev.bump.alloc(NixStr::new(
            ev.alloc_str(&self.bytes),
            context.finish(ev.bump),
        )))
    }
}

impl<'a> Evaluator<'a> {
    /// A string's bytes, computing any lazy hash in them (which hashes its
    /// tree).
    pub fn str_bytes(&self, s: &'a NixStr<'a>) -> Result<&'a [u8]> {
        Ok(self.resolve_hashes(s)?.raw_bytes())
    }

    /// A string's bytes and context, computing any lazy hash in the bytes.
    /// A lazy tree's `Path` element stays in the context as `LazyPath`: it
    /// is position-independent, and the few places that read `Path`
    /// elements need it only once it is known (see
    /// `Evaluator::known_lazy_path`).
    pub fn str_parts(&self, s: &'a NixStr<'a>) -> Result<(&'a [u8], &'a [ContextElem<'a>])> {
        let s = self.resolve_hashes(s)?;
        Ok((s.raw_bytes(), s.raw_context()))
    }

    /// The string with its lazy hashes computed, its context otherwise as
    /// it is.
    fn resolve_hashes(&self, s: &'a NixStr<'a>) -> Result<&'a NixStr<'a>> {
        if !matches!(s.raw_context().first(), Some(ContextElem::LazyHash { .. })) {
            return Ok(s);
        }
        let mut bytes = s.raw_bytes().to_vec();
        for h in lazy_hashes(s) {
            let sp = self.materialize(LazyTreeId(h.tree))?;
            bytes[h.offset..h.offset + HASH_LEN].copy_from_slice(sp.hash_part().as_bytes());
        }
        Ok(self.bump.alloc(NixStr::new(
            self.alloc_str(&bytes),
            context_without_hashes(s),
        )))
    }

    /// For the places that warn when some text names a `Path` element's
    /// store path (a reference without context): lazy tree `tree`'s store
    /// path basename, if it is known, to look for.
    ///
    /// While it is not known, no text can name it: the only way to the
    /// hash is resolving a lazy string, which records it, and a copy of
    /// the whole tree records it too (`note_lazy_root_hashed`). What this
    /// misses is a different store object with byte-identical contents and
    /// the same name: Nix without lazy trees would warn about that one.
    pub(crate) fn known_lazy_path(&self, tree: u32) -> Option<&'a str> {
        let trees = self.lazy_trees.borrow();
        let sp = trees.get(tree as usize)?.store_path.get()?;
        Some(self.arena_str(sp.as_str()))
    }

    /// A string's bytes and context with everything lazy resolved, `Path`
    /// elements included: for handing the context (encoded) to callers
    /// outside the evaluator.
    pub fn str_parts_resolved(
        &self,
        s: &'a NixStr<'a>,
    ) -> Result<(&'a [u8], &'a [ContextElem<'a>])> {
        let s = self.resolve_str(s)?;
        Ok((s.raw_bytes(), s.raw_context()))
    }

    /// The string with its lazy hashes and lazy context resolved: for
    /// showing its context (encoded) outside the evaluator.
    pub fn resolve_str(&self, s: &'a NixStr<'a>) -> Result<&'a NixStr<'a>> {
        if !s.is_lazy() {
            return Ok(s);
        }
        let mut bytes = s.raw_bytes().to_vec();
        let mut context = StringContext::new();
        for c in s.raw_context() {
            match *c {
                ContextElem::LazyHash { offset, tree } => {
                    let sp = self.materialize(LazyTreeId(tree))?;
                    let o = offset as usize;
                    bytes[o..o + HASH_LEN].copy_from_slice(sp.hash_part().as_bytes());
                }
                ContextElem::LazyPath(tree) => {
                    let sp = self.materialize(LazyTreeId(tree))?;
                    context.insert(ContextElem::Path(self.arena_str(sp.as_str())));
                }
                other => {
                    context.insert(other);
                }
            }
        }
        Ok(self.bump.alloc(NixStr::new(
            self.alloc_str(&bytes),
            context.finish(self.bump),
        )))
    }

    /// The text of a path (absolute, on the root accessor) in a lazy tree
    /// whose store path is not known yet: `<store>/<hash>-<name>/…` with a
    /// placeholder hash, and where that hash is. `None` otherwise.
    pub(crate) fn lazy_path_text(&self, path: &[u8]) -> Option<(Vec<u8>, usize, u32)> {
        let (tree, rest) = self.find_lazy(path)?;
        if tree.store_path.get().is_some() {
            return None;
        }
        let id = self.lazy_tree_id(&tree);
        let store = self.config.store_dir.as_str();
        let mut text =
            Vec::with_capacity(store.len() + HASH_LEN + tree.name.len() + rest.len() + 2);
        text.extend_from_slice(store.as_bytes());
        text.push(b'/');
        let hash_at = text.len();
        text.extend(std::iter::repeat_n(FILLER, HASH_LEN));
        text.push(b'-');
        text.extend_from_slice(tree.name.as_bytes());
        text.extend_from_slice(rest);
        Some((text, hash_at, id.0))
    }

    /// Appends the text of a path on the root accessor, keeping a lazy
    /// tree's hash lazy. With `with_context`, a store path gets the `Path`
    /// context Nix gives it.
    pub(crate) fn push_path_text(
        &self,
        out: &mut LazyStrBuilder<'a>,
        path: &[u8],
        with_context: bool,
    ) -> Result<()> {
        if let Some((text, hash_at, tree)) = self.lazy_path_text(path) {
            out.push_lazy_text(&text, hash_at, tree, with_context);
            return Ok(());
        }
        let text = self.logical_path(path)?;
        if with_context {
            if let Some(sp) = self.maybe_store_path(&*text) {
                out.context
                    .insert(ContextElem::Path(self.arena_str(sp.as_str())));
            }
        }
        out.push_bytes(&text);
        Ok(())
    }

    /// A path's text as a string value, keeping a lazy tree's hash lazy
    /// (`toString ./.`, a position's file name, a filter's argument).
    pub(crate) fn lazy_path_string(&self, path: &[u8], with_context: bool) -> Result<Value<'a>> {
        let mut out = LazyStrBuilder::new();
        self.push_path_text(&mut out, path, with_context)?;
        Ok(out.finish(self))
    }

    /// `x == y` for strings that may hold lazy hashes. The same tree's
    /// hash at the same place compares equal without being computed; a
    /// difference elsewhere, or a byte no hash can hold facing a lazy hash,
    /// decides inequality; only otherwise are the hashes computed.
    pub(crate) fn str_eq(&self, x: &'a NixStr<'a>, y: &'a NixStr<'a>) -> Result<bool> {
        if !x.is_lazy() && !y.is_lazy() {
            return Ok(x.raw_bytes() == y.raw_bytes());
        }
        if x.len() != y.len() {
            return Ok(false);
        }
        let hx: Vec<LazyHash> = lazy_hashes(x).collect();
        let hy: Vec<LazyHash> = lazy_hashes(y).collect();
        if hx == hy {
            return Ok(x.raw_bytes() == y.raw_bytes());
        }
        if certainly_differ(x.raw_bytes(), &hx, y.raw_bytes(), &hy) {
            return Ok(false);
        }
        Ok(self.str_bytes(x)? == self.str_bytes(y)?)
    }

    /// `x < y` for strings that may hold lazy hashes, decided without the
    /// hashes whenever [`compare_lazy`] can; otherwise they are computed.
    pub(crate) fn str_less(&self, x: &'a NixStr<'a>, y: &'a NixStr<'a>) -> Result<bool> {
        if !x.is_lazy() && !y.is_lazy() {
            return Ok(x.raw_bytes() < y.raw_bytes());
        }
        let hx: Vec<LazyHash> = lazy_hashes(x).collect();
        let hy: Vec<LazyHash> = lazy_hashes(y).collect();
        if let Some(order) = compare_lazy(x.raw_bytes(), &hx, y.raw_bytes(), &hy) {
            return Ok(order == std::cmp::Ordering::Less);
        }
        Ok(self.str_bytes(x)? < self.str_bytes(y)?)
    }

    /// `s[start..end]` as a string with all of `s`'s context (as Nix's
    /// `substring` keeps it). Lazy hashes wholly inside stay lazy; one the
    /// range cuts through is computed.
    pub(crate) fn str_slice(
        &self,
        s: &'a NixStr<'a>,
        start: usize,
        end: usize,
    ) -> Result<Value<'a>> {
        if !s.is_lazy() {
            return Ok(self.mk_string_view(&s.raw_bytes()[start..end], s.raw_context()));
        }
        let cut = lazy_hashes(s).any(|h| {
            let (a, b) = (h.offset, h.offset + HASH_LEN);
            a < end && b > start && !(a >= start && b <= end)
        });
        if cut {
            let r = self.resolve_str(s)?;
            return Ok(self.mk_string_view(&r.raw_bytes()[start..end], r.raw_context()));
        }
        let mut out = LazyStrBuilder::new();
        out.context.extend_from_slice(context_without_hashes(s));
        out.push_bytes(&s.raw_bytes()[start..end]);
        out.hashes.extend(
            lazy_hashes(s)
                .filter(|h| h.offset >= start && h.offset + HASH_LEN <= end)
                .map(|h| LazyHash {
                    offset: h.offset - start,
                    tree: h.tree,
                }),
        );
        Ok(out.finish(self))
    }

    /// For a string turned into a path (`/. + s`, `import s`, `readFile s`):
    /// the path its text names, at the lazy tree's placeholder, when its
    /// one lazy hash sits where a store path's hash does. `None` when the
    /// string has no lazy hash or it is elsewhere (then it is computed).
    pub(crate) fn lazy_str_path(&self, s: &'a NixStr<'a>) -> Option<Vec<u8>> {
        let mut hashes = lazy_hashes(s);
        let h = hashes.next()?;
        if hashes.next().is_some() {
            return None;
        }
        self.lazy_text_path(s.raw_bytes(), h)
    }

    /// The path `bytes` names at a lazy tree's placeholder, when its lazy
    /// hash `h` sits where a store path's hash does (the store directory,
    /// `/`, the hash, `-` and the tree's name, then nothing or a path below
    /// it, whose bytes are kept as they are).
    pub(crate) fn lazy_text_path(&self, bytes: &[u8], h: LazyHash) -> Option<Vec<u8>> {
        let tree = self.lazy_trees.borrow().get(h.tree as usize)?.clone();
        let store = self.config.store_dir.as_str().as_bytes();
        let after_store = bytes.strip_prefix(store)?.strip_prefix(b"/")?;
        if h.offset != store.len() + 1 {
            return None;
        }
        let rest = after_store
            .get(HASH_LEN..)?
            .strip_prefix(b"-")?
            .strip_prefix(tree.name.as_bytes())?;
        if !(rest.is_empty() || rest[0] == b'/') {
            return None;
        }
        let placeholder = tree.placeholder.as_bytes();
        let mut path = Vec::with_capacity(placeholder.len() + rest.len());
        path.extend_from_slice(placeholder);
        path.extend_from_slice(rest);
        Some(path)
    }

    /// The text of `path + s …` (`bytes`, holding the lazy hashes `hashes`)
    /// as a path. With one hash where a store path's would be right after
    /// the root (`/. + "${r}/sub"`), the path is at the lazy tree's
    /// placeholder, as for a string turned into a path
    /// ([`Evaluator::lazy_str_path`]); otherwise the hashes are computed.
    pub(crate) fn concat_path_text(&self, bytes: &[u8], hashes: &[LazyHash]) -> Result<Vec<u8>> {
        if let [h] = *hashes {
            let store_prefix = self.config.store_dir.as_str().len() + 1;
            if let Some(start) = h.offset.checked_sub(store_prefix) {
                let at_root = CanonPath::new(&bytes[..start]).is_ok_and(|prefix| prefix.is_root());
                let lazy = LazyHash {
                    offset: h.offset - start,
                    tree: h.tree,
                };
                if let Some(path) = at_root
                    .then(|| self.lazy_text_path(&bytes[start..], lazy))
                    .flatten()
                {
                    return Ok(path);
                }
            }
        }
        let mut resolved = bytes.to_vec();
        for h in hashes {
            let sp = self.materialize(LazyTreeId(h.tree))?;
            resolved[h.offset..h.offset + HASH_LEN].copy_from_slice(sp.hash_part().as_bytes());
        }
        Ok(resolved)
    }
}

/// Whether two equally long strings differ for certain, whatever their
/// lazy hashes turn out to be.
pub(crate) fn certainly_differ(x: &[u8], hx: &[LazyHash], y: &[u8], hy: &[LazyHash]) -> bool {
    let lazy =
        |hs: &[LazyHash], i: usize| hs.iter().any(|h| i >= h.offset && i < h.offset + HASH_LEN);
    (0..x.len()).any(|i| match (lazy(hx, i), lazy(hy, i)) {
        (false, false) => x[i] != y[i],
        (true, false) => !in_hash_alphabet(y[i]),
        (false, true) => !in_hash_alphabet(x[i]),
        (true, true) => false,
    })
}

impl<'a> Evaluator<'a> {
    /// Whether turning `v` into text could involve a lazy hash: a string
    /// holding one, or a path into a lazy tree whose hash is unknown. The
    /// operations that keep lazy hashes take their usual, cheaper route
    /// for everything else.
    pub(crate) fn may_be_lazy_text(&self, v: Value<'a>) -> bool {
        match v {
            Value::String(s) => s.is_lazy(),
            Value::Path(p) => {
                p.accessor == self.root_accessor
                    && self
                        .find_lazy(p.path)
                        .is_some_and(|(t, _)| t.store_path.get().is_none())
            }
            _ => false,
        }
    }

    /// `v`'s text as a string value with lazy hashes kept, for an
    /// operation that keeps them (`coerce_into_lazy` with its flags).
    pub(crate) fn lazy_text_value(
        &self,
        pos: SrcPos,
        v: Value<'a>,
        error_ctx: &str,
        canonicalize_path: bool,
    ) -> Result<&'a NixStr<'a>> {
        let mut out = LazyStrBuilder::new();
        self.coerce_into_lazy(
            pos,
            v,
            &mut out,
            error_ctx,
            Coercion {
                more: false,
                copy_to_store: false,
                canonicalize: canonicalize_path,
            },
        )?;
        match out.finish(self) {
            Value::String(s) => Ok(s),
            _ => unreachable!("a built string is a string"),
        }
    }
}

/// What byte `i` of a string is: known, or character `index` of lazy tree
/// `tree`'s hash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LazyByte {
    Known(u8),
    Hash { tree: u32, index: usize },
}

fn lazy_byte(bytes: &[u8], hashes: &[LazyHash], i: usize) -> LazyByte {
    match hashes
        .iter()
        .find(|h| i >= h.offset && i < h.offset + HASH_LEN)
    {
        Some(h) => LazyByte::Hash {
            tree: h.tree,
            index: i - h.offset,
        },
        None => LazyByte::Known(bytes[i]),
    }
}

/// How `x` compares to `y` byte-wise, when that follows from what is known
/// about their lazy hashes: equal where both hold the same character of
/// the same tree's hash, and a hash character (a digit or lowercase
/// letter) is greater than any byte below `0` and less than any above `z`.
/// `None` when the order turns on a hash character.
pub(crate) fn compare_lazy(
    x: &[u8],
    hx: &[LazyHash],
    y: &[u8],
    hy: &[LazyHash],
) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let hash_vs = |c: u8| -> Option<Ordering> {
        if c < b'0' {
            Some(Ordering::Greater)
        } else if c > b'z' {
            Some(Ordering::Less)
        } else {
            None
        }
    };
    for i in 0..x.len().min(y.len()) {
        match (lazy_byte(x, hx, i), lazy_byte(y, hy, i)) {
            (LazyByte::Known(a), LazyByte::Known(b)) => {
                if a != b {
                    return Some(a.cmp(&b));
                }
            }
            (a @ LazyByte::Hash { .. }, b @ LazyByte::Hash { .. }) => {
                if a != b {
                    return None;
                }
            }
            (LazyByte::Hash { .. }, LazyByte::Known(b)) => return hash_vs(b),
            (LazyByte::Known(a), LazyByte::Hash { .. }) => {
                return hash_vs(a).map(Ordering::reverse)
            }
        }
    }
    Some(x.len().cmp(&y.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn h(offset: usize, tree: u32) -> LazyHash {
        LazyHash { offset, tree }
    }

    fn lazy(prefix: &str, suffix: &str) -> Vec<u8> {
        let mut v = prefix.as_bytes().to_vec();
        v.extend(std::iter::repeat_n(FILLER, HASH_LEN));
        v.extend_from_slice(suffix.as_bytes());
        v
    }

    #[test]
    fn placeholders_never_look_like_hashes() {
        assert!(!in_hash_alphabet(FILLER));
        assert!("0123456789abcdfghijklmnpqrsvwxyz"
            .bytes()
            .all(in_hash_alphabet));
    }

    #[test]
    fn order_is_decided_without_hashes_where_it_can_be() {
        let a = lazy("/nix/store/", "-source/a");
        let b = lazy("/nix/store/", "-source/b");
        let ha = [h(11, 0)];
        // Same tree, same place: the rest decides.
        assert_eq!(compare_lazy(&a, &ha, &b, &ha), Some(Ordering::Less));
        // A difference before the hash decides.
        assert_eq!(
            compare_lazy(&a, &ha, b"/home/x", &[]),
            Some(Ordering::Greater)
        );
        // A byte outside `0`..`z` facing the hash decides.
        assert_eq!(
            compare_lazy(&a, &ha, b"/nix/store/-", &[]),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_lazy(&a, &ha, b"/nix/store/~", &[]),
            Some(Ordering::Less)
        );
        // A prefix of the known part is shorter, hence less.
        assert_eq!(
            compare_lazy(&a, &ha, b"/nix/store/", &[]),
            Some(Ordering::Greater)
        );
        // Another store path, or another tree's hash, needs the hash.
        assert_eq!(compare_lazy(&a, &ha, b"/nix/store/abc", &[]), None);
        assert_eq!(compare_lazy(&a, &ha, &b, &[h(11, 1)]), None);
    }

    #[test]
    fn inequality_is_certain_only_where_bytes_or_alphabets_differ() {
        let a = lazy("/nix/store/", "-source");
        let ha = [h(11, 0)];
        let mut other = a.clone();
        other[0] = b'x';
        assert!(certainly_differ(&a, &ha, &other, &ha));
        let mut slash = a.clone();
        slash[20] = b'/';
        assert!(certainly_differ(&a, &ha, &slash, &[]));
        let mut digit = a.clone();
        digit[11..43].fill(b'0');
        assert!(!certainly_differ(&a, &ha, &digit, &[]));
    }
}
