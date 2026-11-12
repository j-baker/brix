//! The compiled form of a Nix source file.
//!
//! The compiler (`crate::compile`) turns the parser's AST into a flat,
//! index-based representation: a `Vec<Node>` addressed by [`NodeId`], with
//! side tables for the pieces that nodes refer to by range. Every variable
//! reference is already resolved to a `(level, displacement)` pair or to a
//! `with` lookup, exactly as Nix's `bindVars` does, so the evaluator never
//! looks names up in scopes.
//!
//! The representation deliberately contains no pointers, which is what lets
//! a compiled-input image store it (`image/codec.rs` encodes these tables).

pub use brix_syntax::ast::{BinOp, PathAnchor, Pos};
use bstr::BString;

use crate::symbol::Symbol;

/// Index of a node in [`Code::nodes`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(pub u32);

/// Index into [`Code::strings`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LitId(pub u32);

/// Index into [`Code::attr_sets`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttrsId(pub u32);

/// Index into [`Code::lambdas`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LambdaId(pub u32);

/// A contiguous range in one of the side tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub start: u32,
    pub len: u32,
}

impl Range {
    pub fn iter(self) -> impl Iterator<Item = usize> {
        (self.start as usize)..(self.start as usize + self.len as usize)
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Where a variable's value lives at run time. (`ExprVar` in Nix.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VarRef {
    /// Go `level` environments up and take slot `displ`.
    Env { level: u32, displ: u32 },
    /// Not statically bound: look the name up in the nearest enclosing
    /// `with` (the [`Node::With`] at `with`), whose environment is `level`
    /// frames up. If that set lacks the name, continue with that `with`'s
    /// `parent_with`.
    With { level: u32, with: NodeId },
}

/// One component of a selection path. (`AttrName` in Nix.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrName {
    Static(Symbol),
    Dynamic(NodeId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrKind {
    Plain,
    Inherited,
    InheritedFrom,
}

impl AttrKind {
    /// `AttrDef::chooseByKind`: what a binding of this kind is evaluated
    /// in. `plain` for `name = expr;` (the set's own scope when it is
    /// recursive), `inherited` for `inherit name;` (the enclosing scope),
    /// and `inherited_from` for `inherit (from) name;` (the scope holding
    /// the `from` values, which a set with such bindings always has).
    #[inline(always)]
    pub fn choose<T>(self, plain: T, inherited: T, inherited_from: Option<T>) -> T {
        match self {
            AttrKind::Plain => plain,
            AttrKind::Inherited => inherited,
            AttrKind::InheritedFrom => {
                inherited_from.expect("a set with `inherit (from)` has an inherit-from scope")
            }
        }
    }
}

/// A static attribute definition. (`ExprAttrs::AttrDef`.)
#[derive(Clone, Copy, Debug)]
pub struct AttrDef {
    pub name: Symbol,
    pub pos: Pos,
    pub kind: AttrKind,
    /// `Plain`: the value expression. `Inherited`: a [`Node::Var`] for the
    /// name. `InheritedFrom`: a [`Node::Select`] on a [`Node::InheritFrom`].
    pub expr: NodeId,
    /// Slot in the environment created by a `rec` set or `let`.
    pub displ: u32,
}

/// `${name} = value;` (`ExprAttrs::DynamicAttrDef`.)
#[derive(Clone, Copy, Debug)]
pub struct DynamicAttrDef {
    pub name: NodeId,
    pub value: NodeId,
    pub pos: Pos,
}

/// The bindings of an attribute set or `let`. (`ExprAttrs`.)
#[derive(Clone, Debug)]
pub struct AttrSet {
    pub recursive: bool,
    pub pos: Pos,
    /// Sorted by `name`, as Nix's `std::map<Symbol, AttrDef>` is; `displ`
    /// is the index in this vector.
    pub attrs: Vec<AttrDef>,
    pub dynamic_attrs: Vec<DynamicAttrDef>,
    /// The `from` expressions of `inherit (from) …;` clauses.
    pub inherit_from: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug)]
pub struct Formal {
    pub pos: Pos,
    pub name: Symbol,
    pub default: Option<NodeId>,
}

#[derive(Clone, Debug)]
pub struct Formals {
    /// Sorted by `(name, pos)`, as Nix's `validateFormals` sorts them. The
    /// environment slot of a formal is its index here, offset by one when
    /// the lambda also has an `@` argument.
    pub formals: Vec<Formal>,
    pub ellipsis: bool,
}

/// A function definition. (`ExprLambda`.)
#[derive(Clone, Debug)]
pub struct Lambda {
    pub pos: Pos,
    /// Set when the lambda is the value of an attribute, as `setName` does.
    pub name: Option<Symbol>,
    /// The `@` name, or the plain argument name.
    pub arg: Option<Symbol>,
    pub formals: Option<Formals>,
    pub body: NodeId,
}

/// One node of the expression tree. Mirrors `nixexpr.hh` after the
/// parser's desugaring; see [`brix_syntax::ast::Expr`].
#[derive(Clone, Copy, Debug)]
pub enum Node {
    Int(i64),
    Float(f64),
    Str(LitId),
    Path {
        anchor: PathAnchor,
        lit: LitId,
    },
    Var {
        pos: Pos,
        name: Symbol,
        var: VarRef,
    },
    /// The pseudo-variable for `AttrSet::inherit_from[displ]`: slot `displ`
    /// of the extra environment that holds the `inherit (from)` sources.
    InheritFrom {
        pos: Pos,
        displ: u32,
    },
    Select {
        pos: Pos,
        subject: NodeId,
        /// Range in [`Code::attr_names`].
        path: Range,
        default: Option<NodeId>,
    },
    HasAttr {
        subject: NodeId,
        path: Range,
    },
    Attrs(AttrsId),
    List {
        /// Range in [`Code::node_lists`].
        elems: Range,
    },
    Lambda(LambdaId),
    Call {
        pos: Pos,
        fun: NodeId,
        /// Range in [`Code::node_lists`].
        args: Range,
    },
    Let {
        attrs: AttrsId,
        body: NodeId,
    },
    With {
        pos: Pos,
        attrs: NodeId,
        body: NodeId,
        /// How many environments up the enclosing `with`'s environment is
        /// from this one's, or 0 if there is none. (`ExprWith::prevWith`.)
        prev_with: u32,
        /// The enclosing `with`. (`ExprWith::parentWith`.)
        parent_with: Option<NodeId>,
    },
    If {
        pos: Pos,
        cond: NodeId,
        then_: NodeId,
        else_: NodeId,
    },
    Assert {
        pos: Pos,
        cond: NodeId,
        body: NodeId,
    },
    Not(NodeId),
    BinOp {
        pos: Pos,
        op: BinOp,
        lhs: NodeId,
        rhs: NodeId,
    },
    ConcatStrings {
        pos: Pos,
        force_string: bool,
        /// Range in [`Code::parts`].
        parts: Range,
    },
    CurPos(Pos),
}

/// Where a piece of code came from, for positions in messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A file: `path` is its absolute path on the root file system (what
    /// `__curPos.file` reports), `display` how its accessor shows it in
    /// messages (e.g. `/pwd/foo.nix` or `«github:owner/repo»/foo.nix`).
    /// Both are bytes, as a file's name need not be UTF-8.
    File { display: BString, path: BString },
    /// `nix-instantiate --expr`, shown as `«string»`.
    String,
    /// Standard input, shown as `«stdin»`.
    Stdin,
    /// The evaluator's own helper expressions, shown as
    /// `«nix-internal»/<name>`.
    Internal { name: String },
}

impl Origin {
    /// How a position in this source names it (`Pos::Origin`'s printing):
    /// bytes, written as they are.
    pub fn display(&self) -> BString {
        match self {
            Origin::File { display, .. } => display.clone(),
            Origin::String => BString::from("«string»"),
            Origin::Stdin => BString::from("«stdin»"),
            Origin::Internal { name } => BString::from(format!("«nix-internal»/{name}")),
        }
    }
}

/// A compiled source file.
#[derive(Clone, Debug)]
pub struct Code {
    /// The expression the file evaluates to.
    pub root: NodeId,
    pub nodes: Vec<Node>,
    pub strings: Vec<Vec<u8>>,
    pub attr_sets: Vec<AttrSet>,
    pub lambdas: Vec<Lambda>,
    pub attr_names: Vec<AttrName>,
    pub node_lists: Vec<NodeId>,
    pub parts: Vec<(Pos, NodeId)>,
}

impl Code {
    pub fn string(&self, id: LitId) -> &[u8] {
        &self.strings[id.0 as usize]
    }
}
