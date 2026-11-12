//! The abstract syntax of Nix expressions.
//!
//! This mirrors Nix's `nixexpr.hh` node for node, after the desugarings its
//! parser performs. The evaluator consumes exactly this: there is no
//! `Minus`, `Times`, `LessThan` or unary negation node because Nix's parser
//! rewrites those into calls to `__sub`, `__mul`, `__div` and `__lessThan`,
//! and `<nixpkgs>` into `__findFile __nixPath "nixpkgs"`. Reproducing the
//! desugaring here keeps every observable behaviour (error messages,
//! evaluation order, `builtins.unsafeGetAttrPos`) identical.
//!
//! Positions are byte offsets into the source text ([`Pos`]). Line and
//! column are derived on demand by [`crate::SourceFile`].

use std::fmt;

use hipstr::HipByt;

/// A byte offset into the source file, or [`Pos::NONE`] when Nix would
/// record `noPos`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pos(pub u32);

impl Default for Pos {
    fn default() -> Self {
        Pos::NONE
    }
}

impl Pos {
    pub const NONE: Pos = Pos(u32::MAX);

    pub fn is_none(self) -> bool {
        self == Pos::NONE
    }
}

impl std::fmt::Debug for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_none() {
            f.write_str("Pos(none)")
        } else {
            write!(f, "Pos({})", self.0)
        }
    }
}

/// An identifier or attribute name. Bytes, as in Nix: a quoted name
/// (`{ "\xff" = 1; }` in a file that is not UTF-8) keeps them, so names
/// that differ only in bytes that are not UTF-8 stay different. Not
/// interned here; the evaluator interns names into symbols when it
/// compiles the AST.
///
/// Names are made for every identifier the parser reads and cloned when
/// attribute paths are merged, so they are small strings: up to 23
/// bytes, which is nearly every name, are kept inline without an
/// allocation, and a longer name is shared by reference count, so a
/// clone never copies it. A `Name` is 24 bytes, and `Option<Name>` too.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(HipByt<'static>);

impl Name {
    pub fn new(bytes: &[u8]) -> Name {
        Name(HipByt::from(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Name {
        Name::new(s.as_bytes())
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl fmt::Debug for Name {
    /// Quoted, with the bytes that are not printable ASCII escaped.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self.as_bytes().escape_ascii())
    }
}

/// One component of an attribute path: `a` or `"a"` is static, `${e}` is
/// dynamic. (`AttrName` in Nix.)
#[derive(Clone, Debug, PartialEq)]
pub enum AttrName {
    Static(Name),
    Dynamic(Box<Expr>),
}

/// How an attribute got its value. (`ExprAttrs::AttrDef::Kind` in Nix.)
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    /// `name = expr;`
    Plain(Box<Expr>),
    /// `inherit name;` — the value is the variable `name` in the enclosing
    /// scope. (Nix stores an `ExprVar` here; the position is the attribute's.)
    Inherited,
    /// `inherit (from) name;` — `from` is `AttrSet::inherit_from[index]`,
    /// evaluated once and shared by every name inherited from it.
    InheritedFrom { index: u32 },
}

/// A static attribute definition inside `{ … }`, `rec { … }` or `let`.
#[derive(Clone, Debug, PartialEq)]
pub struct AttrDef {
    pub name: Name,
    pub pos: Pos,
    pub value: AttrValue,
}

/// `${nameExpr} = valueExpr;`
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicAttrDef {
    pub name: Box<Expr>,
    pub value: Box<Expr>,
    pub pos: Pos,
}

/// The bindings of an attribute set or `let`. (`ExprAttrs` in Nix.)
///
/// Nested attribute paths are already merged, as Nix's `addAttr` does at
/// parse time: `{ a.b = 1; a.c = 2; }` is one attribute `a` whose value is a
/// non-recursive set with `b` and `c`. Definitions appear in source order of
/// their *first* occurrence; duplicate static names are a parse error.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AttrSet {
    pub recursive: bool,
    pub pos: Pos,
    pub attrs: Vec<AttrDef>,
    pub dynamic_attrs: Vec<DynamicAttrDef>,
    /// The `from` expressions of `inherit (from) …;` clauses, in source
    /// order. Referenced by index from [`AttrValue::InheritedFrom`].
    pub inherit_from: Vec<Expr>,
}

/// One formal parameter in `{ name ? default, … }:`.
#[derive(Clone, Debug, PartialEq)]
pub struct Formal {
    pub pos: Pos,
    pub name: Name,
    pub default: Option<Box<Expr>>,
}

/// The `{ … }` part of a function head. Formals are kept in source order
/// (Nix sorts them by symbol; the evaluator does whatever ordering it needs).
/// Duplicate formals are a parse error.
#[derive(Clone, Debug, PartialEq)]
pub struct Formals {
    pub formals: Vec<Formal>,
    pub ellipsis: bool,
}

/// Where a path literal is anchored. Nix resolves this at parse time to an
/// absolute path on one of two accessors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathAnchor {
    /// `/abs/path` — relative to the root file system accessor.
    Root,
    /// `./rel` or `rel/ative` — relative to the directory containing the
    /// source file, on that file's accessor.
    Source,
}

/// A binary operator that survives desugaring as its own node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// `==`
    Eq,
    /// `!=`
    NEq,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `->`
    Impl,
    /// `//`
    Update,
    /// `++`
    ConcatLists,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// `123`. Overflowing literals are a parse error, as in Nix.
    Int { pos: Pos, value: i64 },
    /// `1.5`, `.5e3`
    Float { pos: Pos, value: f64 },
    /// A string with no interpolation, already unescaped. URI literals and
    /// the string halves of `${…}` sequences land here too.
    String { pos: Pos, value: Vec<u8> },
    /// A path literal, absolute after resolution against the source
    /// file's directory. A trailing `/` in the literal is kept. Bytes, as
    /// Nix's paths are: the directory need not be UTF-8.
    Path {
        pos: Pos,
        anchor: PathAnchor,
        path: Vec<u8>,
    },
    /// A variable reference. `__curPos` never appears here (see
    /// [`Expr::CurPos`]).
    Var { pos: Pos, name: Name },
    /// `subject.a.b.c` or `subject.a.b.c or default`. Also produced by
    /// `let { body = …; }` (as `(rec { … }).body`). An `inherit (from) a;`
    /// is not a `Select` but [`AttrValue::InheritedFrom`].
    Select {
        pos: Pos,
        subject: Box<Expr>,
        path: Vec<AttrName>,
        default: Option<Box<Expr>>,
    },
    /// `subject ? a.b.c`
    HasAttr {
        subject: Box<Expr>,
        path: Vec<AttrName>,
    },
    /// `{ … }` or `rec { … }`. Boxed, like `Let`'s set and a lambda's formals,
    /// so that `Expr` stays 48 bytes: the parser moves expressions around
    /// by value constantly, and an inline `AttrSet` made every node 88.
    Attrs(Box<AttrSet>),
    /// `[ a b c ]`
    List { pos: Pos, elems: Vec<Expr> },
    /// `x: body`, `{ a, b ? 1, ... }: body`, `args @ { … }: body`.
    Lambda {
        pos: Pos,
        /// The `@` name or the plain argument name.
        arg: Option<Name>,
        formals: Option<Box<Formals>>,
        body: Box<Expr>,
    },
    /// `f a b c`. Nested applications are flattened into one node with
    /// several arguments, as Nix's `makeCall` does. `pos` is the position of
    /// the whole application (its first token).
    Call {
        pos: Pos,
        fun: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `let … in body`. `attrs.recursive` is irrelevant here; dynamic
    /// attributes are a parse error.
    Let {
        attrs: Box<AttrSet>,
        body: Box<Expr>,
    },
    /// `with attrs; body`
    With {
        pos: Pos,
        attrs: Box<Expr>,
        body: Box<Expr>,
    },
    /// `if cond then then_ else else_`
    If {
        pos: Pos,
        cond: Box<Expr>,
        then_: Box<Expr>,
        else_: Box<Expr>,
    },
    /// `assert cond; body`
    Assert {
        pos: Pos,
        cond: Box<Expr>,
        body: Box<Expr>,
    },
    /// `!e`. Also produced for `<=` and `>=`.
    Not(Box<Expr>),
    /// The operators Nix keeps as nodes. `pos` is the operator's position.
    BinOp {
        pos: Pos,
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// String interpolation `"a${b}c"`, indented strings, interpolated
    /// paths `./foo/${bar}`, and the `+` operator. With `force_string` the
    /// result is always a string (string literals); without it the result
    /// type follows the first part (`+` and paths): int, float, path or
    /// string, exactly as `ExprConcatStrings::eval`.
    ConcatStrings {
        pos: Pos,
        force_string: bool,
        parts: Vec<(Pos, Expr)>,
    },
    /// `__curPos`
    CurPos { pos: Pos },
}

impl Expr {
    /// The position Nix's `Expr::getPos` reports for this node.
    pub fn pos(&self) -> Pos {
        match self {
            Expr::Var { pos, .. }
            | Expr::Select { pos, .. }
            | Expr::Lambda { pos, .. }
            | Expr::Call { pos, .. }
            | Expr::With { pos, .. }
            | Expr::If { pos, .. }
            | Expr::Assert { pos, .. }
            | Expr::BinOp { pos, .. }
            | Expr::ConcatStrings { pos, .. }
            | Expr::CurPos { pos } => *pos,
            Expr::Attrs(set) => set.pos,
            Expr::HasAttr { subject, .. } => subject.pos(),
            Expr::Not(e) => e.pos(),
            Expr::List { elems, .. } => elems.first().map_or(Pos::NONE, Expr::pos),
            // Nix stores no position on literals: `getPos` is `noPos` for
            // them even though the parser records where they were.
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::String { .. }
            | Expr::Path { .. }
            | Expr::Let { .. } => Pos::NONE,
        }
    }
}

/// Dropping an expression is iterative. The compiler's drop glue would
/// recurse once per nesting level, and source can nest far deeper than
/// any stack (the parser and the evaluator grow theirs; a destructor
/// cannot): a generated 100,000-deep `else if` chain overflowed while
/// being dropped. Children that have children of their own are moved to
/// an explicit stack, a leaf left in their place, so every node is
/// dropped with its children already detached.
impl Drop for Expr {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.detach_children(&mut pending);
        while let Some(mut e) = pending.pop() {
            e.detach_children(&mut pending);
        }
    }
}

impl Expr {
    /// A node without children, the stand-in for a detached child.
    const DETACHED: Expr = Expr::CurPos { pos: Pos::NONE };

    /// Moves every child of this node that has children itself to
    /// `out`. Leaves stay: dropping them recurses no further.
    fn detach_children(&mut self, out: &mut Vec<Expr>) {
        let mut detach = |e: &mut Expr| {
            if e.has_children() {
                out.push(std::mem::replace(e, Expr::DETACHED));
            }
        };
        match self {
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::String { .. }
            | Expr::Path { .. }
            | Expr::Var { .. }
            | Expr::CurPos { .. } => {}
            Expr::Select {
                subject,
                path,
                default,
                ..
            } => {
                detach(subject);
                detach_path(path, &mut detach);
                if let Some(d) = default {
                    detach(d);
                }
            }
            Expr::HasAttr { subject, path } => {
                detach(subject);
                detach_path(path, &mut detach);
            }
            Expr::Attrs(set) => detach_set(set, &mut detach),
            Expr::List { elems, .. } => elems.iter_mut().for_each(detach),
            Expr::Lambda { formals, body, .. } => {
                if let Some(f) = formals {
                    for formal in &mut f.formals {
                        if let Some(d) = &mut formal.default {
                            detach(d);
                        }
                    }
                }
                detach(body);
            }
            Expr::Call { fun, args, .. } => {
                detach(fun);
                args.iter_mut().for_each(detach);
            }
            Expr::Let { attrs, body } => {
                detach_set(attrs, &mut detach);
                detach(body);
            }
            Expr::With { attrs, body, .. } => {
                detach(attrs);
                detach(body);
            }
            Expr::If {
                cond, then_, else_, ..
            } => {
                detach(cond);
                detach(then_);
                detach(else_);
            }
            Expr::Assert { cond, body, .. } => {
                detach(cond);
                detach(body);
            }
            Expr::Not(e) => detach(e),
            Expr::BinOp { lhs, rhs, .. } => {
                detach(lhs);
                detach(rhs);
            }
            Expr::ConcatStrings { parts, .. } => parts.iter_mut().for_each(|(_, e)| detach(e)),
        }
    }

    fn has_children(&self) -> bool {
        !matches!(
            self,
            Expr::Int { .. }
                | Expr::Float { .. }
                | Expr::String { .. }
                | Expr::Path { .. }
                | Expr::Var { .. }
                | Expr::CurPos { .. }
        )
    }
}

fn detach_path(path: &mut [AttrName], detach: &mut impl FnMut(&mut Expr)) {
    for p in path {
        if let AttrName::Dynamic(e) = p {
            detach(e);
        }
    }
}

fn detach_set(set: &mut AttrSet, detach: &mut impl FnMut(&mut Expr)) {
    for def in &mut set.attrs {
        if let AttrValue::Plain(e) = &mut def.value {
            detach(e);
        }
    }
    for d in &mut set.dynamic_attrs {
        detach(&mut d.name);
        detach(&mut d.value);
    }
    set.inherit_from.iter_mut().for_each(detach);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser moves names and expressions around by value: a name is
    /// as small as the `SmolStr` it replaced, and an expression stays 48
    /// bytes (see [`Expr::Attrs`]).
    #[test]
    fn names_and_expressions_stay_small() {
        assert_eq!(std::mem::size_of::<Name>(), 24);
        assert_eq!(std::mem::size_of::<Option<Name>>(), 24);
        assert_eq!(std::mem::size_of::<AttrName>(), 24);
        assert_eq!(std::mem::size_of::<Expr>(), 48);
    }

    /// Names are bytes: two that differ only in bytes that are not UTF-8
    /// stay apart, and a debug view escapes them.
    #[test]
    fn names_are_bytes() {
        let a = Name::new(b"\xff");
        let b = Name::new(b"\xfe");
        assert_ne!(a, b);
        assert_eq!(a.as_bytes(), b"\xff");
        assert_eq!(format!("{a:?}"), r#""\xff""#);
        let long = Name::new(&[b'x'; 40]);
        assert_eq!(long.clone().as_bytes(), &[b'x'; 40]);
        assert_eq!(Name::from("or"), "or");
    }
}
