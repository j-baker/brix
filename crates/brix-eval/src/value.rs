//! Run-time values.
//!
//! The model is Nix's own (`value.hh`): a [`Value`] is a 16-byte tagged
//! union, mutable *cells* ([`ValueCell`]) hold values and are shared
//! between environments, attribute sets and lists, and forcing a thunk
//! overwrites its cell in place so every sharer sees the result. All
//! run-time objects live in an arena owned by the [`crate::Evaluator`] and
//! borrow its lifetime `'a`; nothing is reference counted and nothing is
//! freed before the evaluator is.

use crate::runtime::cell::Cell;

use brix_store::AccessorId;

/// Index of a loaded [`crate::LinkedCode`] in the evaluator's registry.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodeId(pub u32);

/// A source position: a byte offset in a loaded file. `SrcPos::NONE` is
/// Nix's `noPos`.
///
/// Like Nix's `PosIdx`, this is a single `u32`: every loaded file gets a
/// contiguous range of the position space (its base offset plus its
/// length), and the evaluator maps a position back to its file by binary
/// search over the bases. Keeping it to four bytes matters because every
/// attribute carries one: an [`Attr`] is 16 bytes with this, 24 with a
/// `(file, offset)` pair, and nixpkgs evaluations copy tens of millions
/// of attributes through `//`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SrcPos(pub u32);

impl SrcPos {
    pub const NONE: SrcPos = SrcPos(u32::MAX);

    pub fn is_none(self) -> bool {
        self.0 == u32::MAX
    }

    pub fn is_some(self) -> bool {
        !self.is_none()
    }

    pub fn or(self, other: SrcPos) -> SrcPos {
        if self.is_some() {
            self
        } else {
            other
        }
    }
}

/// One element of a string's context: the store objects a string refers
/// to. (`NixStringContextElem` in Nix.) Store paths are kept as their
/// basename (`<hash>-<name>`), independent of the store directory.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ContextElem<'a> {
    /// Hidden, never part of the context Nix shows (see
    /// [`crate::lazy_tree`]): bytes `offset..offset + 32` of the string
    /// stand for the hash of lazy tree `tree`'s store path, which nobody
    /// has computed. The hidden kinds sort first, so a string with lazy
    /// parts is recognised by its first element.
    LazyHash { offset: u32, tree: u32 },
    /// Hidden: the `Path` element for lazy tree `tree`'s store path.
    LazyPath(u32),
    /// A plain store path: the string refers to that path.
    Opaque(&'a str),
    /// A derivation and its whole closure (`drvPath` of a derivation).
    DrvDeep(&'a str),
    /// One output of a derivation (`outPath` of a derivation). The output
    /// name is bytes, as Nix's are: a floating content-addressed output's
    /// need not be UTF-8.
    Built { drv: &'a str, output: &'a [u8] },
    /// A path inside the store that was mentioned without being copied;
    /// Nix tracks it but it does not count as "having context".
    Path(&'a str),
}

impl<'a> ContextElem<'a> {
    /// Whether this element makes `builtins.hasContext` true.
    pub fn is_real(self) -> bool {
        !matches!(
            self,
            ContextElem::Path(_) | ContextElem::LazyHash { .. } | ContextElem::LazyPath(_)
        )
    }

    /// Whether this is one of the hidden lazy-tree elements.
    pub fn is_lazy(self) -> bool {
        matches!(
            self,
            ContextElem::LazyHash { .. } | ContextElem::LazyPath(_)
        )
    }

    /// Nix's internal encoding, as `getContext`/`appendContext` and
    /// `.drv` inputs see it: `!out!drv`, `=drv`, `@path` or `path`.
    /// Bytes, as an output name is.
    pub fn encode(self) -> Vec<u8> {
        let tagged = |tag: u8, p: &str| {
            let mut out = Vec::with_capacity(1 + p.len());
            out.push(tag);
            out.extend_from_slice(p.as_bytes());
            out
        };
        match self {
            ContextElem::Opaque(p) => p.as_bytes().to_vec(),
            ContextElem::DrvDeep(p) => tagged(b'=', p),
            ContextElem::Built { drv, output } => {
                let mut out = Vec::with_capacity(2 + output.len() + drv.len());
                out.push(b'!');
                out.extend_from_slice(output);
                out.push(b'!');
                out.extend_from_slice(drv.as_bytes());
                out
            }
            ContextElem::Path(p) => tagged(b'@', p),
            ContextElem::LazyHash { .. } | ContextElem::LazyPath(_) => {
                unreachable!("lazy context is resolved before it is shown")
            }
        }
    }
}

/// A string value with its context.
///
/// The fields are private because a string may hold lazy hashes (see
/// [`crate::lazy_tree`]), whose bytes are placeholders. Code reads a string
/// through [`crate::Evaluator::str_bytes`] or
/// [`crate::Evaluator::str_parts`], which compute any lazy hash first, or,
/// when it handles lazy hashes itself, through the `raw_` accessors.
#[derive(Debug)]
pub struct NixStr<'a> {
    bytes: &'a [u8],
    /// Sorted and deduplicated; hidden lazy elements first.
    context: &'a [ContextElem<'a>],
}

impl<'a> NixStr<'a> {
    pub(crate) fn new(bytes: &'a [u8], context: &'a [ContextElem<'a>]) -> NixStr<'a> {
        NixStr { bytes, context }
    }

    /// Whether the string holds a lazy hash or a lazy path's context.
    pub fn is_lazy(&self) -> bool {
        self.context.first().is_some_and(|c| c.is_lazy())
    }

    pub fn has_context(&self) -> bool {
        self.context.iter().any(|c| c.is_real())
    }

    /// The length in bytes, exact even with lazy hashes in it.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes, lazy hashes as placeholders: only for code that handles
    /// them ([`crate::lazy_tree`]).
    pub(crate) fn raw_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The context with its hidden lazy elements first.
    pub(crate) fn raw_context(&self) -> &'a [ContextElem<'a>] {
        self.context
    }
}

/// A path value: an absolute canonical path on one accessor. The path is
/// bytes, as Nix's are: `/a + "\xff"` is the path `/a\xff`, and a file
/// name read from a tree is kept as it is, UTF-8 or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathVal<'a> {
    pub accessor: AccessorId,
    pub path: &'a [u8],
}

/// Attribute sets live in `crate::bindings`; re-exported here because
/// everything that handles values needs them.
pub use crate::bindings::{Attr, Bindings};

/// A list. Boxed so that [`Value`] stays 16 bytes.
#[derive(Debug)]
pub struct List<'a> {
    pub elems: &'a [ValueRef<'a>],
}

/// A node in the evaluator-wide node index space: every linked file gets
/// a page-aligned range of it (see `crate::code::NODE_PAGE_BITS`), so a
/// suspended expression or a function is `(env, node)` and fits in a
/// [`Value`] without a separate allocation, as Nix's `tThunk` and
/// `tLambda` do. The evaluator maps it back to `(code, NodeId)` with one
/// table lookup ([`crate::Evaluator::resolve_node`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GlobalNode(pub u32);

/// A suspended function application `fun arg`. (`tApp` in Nix.)
#[derive(Debug)]
pub struct App<'a> {
    pub fun: ValueRef<'a>,
    pub arg: ValueRef<'a>,
}

/// The implementation of a built-in function.
#[cfg(not(brix_parallel))]
pub type PrimOpFn<'a> =
    dyn Fn(&crate::Evaluator<'a>, SrcPos, &[ValueRef<'a>]) -> crate::Result<Value<'a>> + 'a;
#[cfg(brix_parallel)]
pub type PrimOpFn<'a> = dyn Fn(&crate::Evaluator<'a>, SrcPos, &[ValueRef<'a>]) -> crate::Result<Value<'a>>
    + Send
    + Sync
    + 'a;

/// A built-in function. (`PrimOp` in Nix.)
pub struct PrimOp<'a> {
    /// The name without the `__` prefix, as shown in messages.
    pub name: &'a str,
    pub arity: u8,
    /// Whether an error inside it gets a "while calling the 'x' builtin"
    /// trace (`PrimOp::addTrace`).
    pub add_trace: bool,
    pub imp: &'a PrimOpFn<'a>,
}

impl std::fmt::Debug for PrimOp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PrimOp({}/{})", self.name, self.arity)
    }
}

/// A partial application of a primop. (`tPrimOpApp`.)
#[derive(Debug)]
pub struct PrimOpApp<'a> {
    pub fun: ValueRef<'a>,
    pub arg: ValueRef<'a>,
}

/// The run-time value. 16 bytes.
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String(&'a NixStr<'a>),
    Path(&'a PathVal<'a>),
    Attrs(Bindings<'a>),
    List(&'a List<'a>),
    /// A function: the [`crate::ir::Node::Lambda`] at `node` closed over
    /// `env`. Recover its definition with
    /// [`crate::Evaluator::lambda_def`].
    Lambda {
        env: Env<'a>,
        node: GlobalNode,
    },
    PrimOp(&'a PrimOp<'a>),
    PrimOpApp(&'a PrimOpApp<'a>),
    /// Not yet evaluated: the expression `node` in environment `env`.
    Thunk {
        env: Env<'a>,
        node: GlobalNode,
    },
    /// A suspended application, also not yet evaluated.
    App(&'a App<'a>),
    /// Being evaluated: forcing it again is infinite recursion.
    Blackhole,
    /// Evaluation failed; forcing it again rethrows the same error.
    Failed(&'a crate::Error),
}

const _: () = assert!(std::mem::size_of::<Value<'static>>() == 16);

/// The coarse type of a value, as `builtins.typeOf` and error messages
/// see it. (`ValueType` in Nix.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueType {
    Int,
    Float,
    Bool,
    Null,
    String,
    Path,
    Attrs,
    List,
    Function,
    Thunk,
}

impl<'a> Value<'a> {
    pub fn value_type(self) -> ValueType {
        match self {
            Value::Int(_) => ValueType::Int,
            Value::Float(_) => ValueType::Float,
            Value::Bool(_) => ValueType::Bool,
            Value::Null => ValueType::Null,
            Value::String(_) => ValueType::String,
            Value::Path(_) => ValueType::Path,
            Value::Attrs(_) => ValueType::Attrs,
            Value::List(_) => ValueType::List,
            Value::Lambda { .. } | Value::PrimOp(_) | Value::PrimOpApp(_) => ValueType::Function,
            Value::Thunk { .. } | Value::App(_) | Value::Blackhole | Value::Failed(_) => {
                ValueType::Thunk
            }
        }
    }

    /// Whether the value is in weak head normal form.
    pub fn is_forced(self) -> bool {
        self.value_type() != ValueType::Thunk
    }

    /// `showType(ValueType)`: the type name in messages, with an article
    /// when `with_article`.
    pub fn type_name(t: ValueType, with_article: bool) -> &'static str {
        match (t, with_article) {
            (ValueType::Int, true) => "an integer",
            (ValueType::Int, false) => "integer",
            (ValueType::Bool, true) => "a Boolean",
            (ValueType::Bool, false) => "Boolean",
            (ValueType::String, true) => "a string",
            (ValueType::String, false) => "string",
            (ValueType::Path, true) => "a path",
            (ValueType::Path, false) => "path",
            (ValueType::Null, _) => "null",
            (ValueType::Attrs, true) => "a set",
            (ValueType::Attrs, false) => "set",
            (ValueType::List, true) => "a list",
            (ValueType::List, false) => "list",
            (ValueType::Function, true) => "a function",
            (ValueType::Function, false) => "function",
            (ValueType::Float, true) => "a float",
            (ValueType::Float, false) => "float",
            (ValueType::Thunk, true) => "a thunk",
            (ValueType::Thunk, false) => "thunk",
        }
    }

    /// `showType(const Value &)`: like [`Value::type_name`] but more
    /// specific for strings with context and built-in functions.
    pub fn show_type(self) -> String {
        match self {
            Value::String(s) if s.has_context() => "a string with context".to_owned(),
            Value::PrimOp(p) => format!("the built-in function '{}'", p.name),
            Value::PrimOpApp(app) => format!(
                "the partially applied built-in function '{}'",
                primop_app_primop(app).map_or("", |p| p.name)
            ),
            Value::Blackhole => "a black hole".to_owned(),
            Value::Thunk { .. } | Value::Failed(_) => "a thunk".to_owned(),
            Value::App(_) => "a function application".to_owned(),
            v => Value::type_name(v.value_type(), true).to_owned(),
        }
    }

    /// `builtins.typeOf`.
    pub fn type_of(self) -> &'static str {
        match self.value_type() {
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Bool => "bool",
            ValueType::Null => "null",
            ValueType::String => "string",
            ValueType::Path => "path",
            ValueType::Attrs => "set",
            ValueType::List => "list",
            ValueType::Function => "lambda",
            ValueType::Thunk => "thunk",
        }
    }

    pub fn as_list(self) -> Option<&'a [ValueRef<'a>]> {
        match self {
            Value::List(l) => Some(l.elems),
            _ => None,
        }
    }
}

/// Follows a partial-application chain down to its primop.
pub fn primop_app_primop<'a>(app: &'a PrimOpApp<'a>) -> Option<&'a PrimOp<'a>> {
    let mut cur = app.fun.get();
    loop {
        match cur {
            Value::PrimOp(p) => return Some(p),
            Value::PrimOpApp(a) => cur = a.fun.get(),
            _ => return None,
        }
    }
}

/// A mutable cell holding a value. Forcing updates the cell in place.
#[derive(Debug)]
pub struct ValueCell<'a>(Cell<Value<'a>>);

/// A shared reference to a cell: what environments, attributes and lists
/// hold. (`Value *` in Nix.)
pub type ValueRef<'a> = &'a ValueCell<'a>;

impl<'a> ValueCell<'a> {
    pub const fn new(v: Value<'a>) -> ValueCell<'a> {
        ValueCell(Cell::new(v))
    }

    pub fn get(&self) -> Value<'a> {
        self.0.get()
    }

    pub fn set(&self, v: Value<'a>) {
        self.0.set(v)
    }
}

/// Environments live in `crate::env`; re-exported here because thunks
/// and lambdas refer to them.
pub use crate::env::Env;
