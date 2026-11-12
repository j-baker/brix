//! Loaded code: [`crate::ir::Code`] moved into the evaluator's arena.
//!
//! Linking copies the compiled file into arena-resident slices (so the
//! evaluator holds plain `&'a` references and nothing needs dropping) and
//! pre-allocates a value cell for every literal node, which is what lets
//! `maybeThunk` hand literals out without allocating.

use crate::runtime::Bump;

use crate::ir::{
    AttrDef, AttrName, AttrsId, Code, DynamicAttrDef, Formal, LambdaId, LitId, Node, NodeId, Pos,
    Range,
};
use crate::symbol::Symbol;
use crate::value::{CodeId, ValueRef};

/// An attribute set's bindings, arena-resident. See [`crate::ir::AttrSet`].
///
/// The compiler sorts `attrs` by the symbol ids *it* saw, and the index
/// is the environment displacement the code refers to. When a file is
/// compiled by this evaluator those ids are the evaluator's own, so the
/// array is in Nix's `std::map<Symbol, …>` order; a file loaded from a
/// compiled-input image was compiled with its own numbering, and the
/// array is then in some other order. `id_order` is the permutation that
/// restores Nix's order for everything observable (the order a `rec`
/// environment is filled in), computed when the file is linked.
#[derive(Debug)]
pub struct AttrSet<'a> {
    pub recursive: bool,
    pub pos: Pos,
    /// The index is the environment displacement; see the type docs for
    /// the order.
    pub attrs: &'a [AttrDef],
    /// Indices into `attrs` in symbol-id order: the order Nix visits the
    /// bindings in, and the order of the [`crate::value::Bindings`] a
    /// static set evaluates to.
    pub id_order: &'a [u32],
    pub dynamic_attrs: &'a [DynamicAttrDef],
    pub inherit_from: &'a [NodeId],
}

impl AttrSet<'_> {
    pub fn find(&self, name: Symbol) -> Option<&AttrDef> {
        self.id_order
            .binary_search_by_key(&name, |&i| self.attrs[i as usize].name)
            .ok()
            .map(|k| &self.attrs[self.id_order[k] as usize])
    }

    /// The definitions in symbol-id order, with their displacements.
    pub fn in_id_order(&self) -> impl Iterator<Item = (usize, &AttrDef)> + '_ {
        self.id_order
            .iter()
            .map(move |&i| (i as usize, &self.attrs[i as usize]))
    }
}

#[derive(Debug)]
pub struct Formals<'a> {
    /// The index (plus one with an `@` argument) is the environment
    /// displacement. Sorted by the compiling symbol table's ids, which are
    /// the evaluator's own unless the file came from an image; `id_order`
    /// gives the order Nix visits formals in.
    pub formals: &'a [Formal],
    /// Indices into `formals` in symbol-id order.
    pub id_order: &'a [u32],
    pub ellipsis: bool,
}

impl Formals<'_> {
    pub fn has(&self, name: Symbol) -> bool {
        self.id_order
            .binary_search_by_key(&name, |&i| self.formals[i as usize].name)
            .is_ok()
    }

    /// The formals in symbol-id order, with their index (the displacement
    /// before the `@` argument offset).
    pub fn in_id_order(&self) -> impl Iterator<Item = (usize, &Formal)> + '_ {
        self.id_order
            .iter()
            .map(move |&i| (i as usize, &self.formals[i as usize]))
    }
}

/// See [`crate::ir::Lambda`].
#[derive(Debug)]
pub struct Lambda<'a> {
    pub pos: Pos,
    pub name: Option<Symbol>,
    pub arg: Option<Symbol>,
    pub formals: Option<Formals<'a>>,
    pub body: NodeId,
}

impl Lambda<'_> {
    pub fn env_size(&self) -> usize {
        usize::from(self.arg.is_some()) + self.formals.as_ref().map_or(0, |f| f.formals.len())
    }
}

/// Node ids are handed out in pages of this many nodes: a file's range in
/// the evaluator-wide node space starts on a page boundary, so a
/// [`crate::value::GlobalNode`] maps to its file with one table lookup
/// (`page_table[node >> NODE_PAGE_BITS]`) and to its [`NodeId`] with one
/// subtraction. The waste is under one page per file.
pub const NODE_PAGE_BITS: u32 = 12;

/// A compiled file loaded into an evaluator.
#[derive(Debug)]
pub struct LinkedCode<'a> {
    pub id: CodeId,
    /// Start of this file's range in the evaluator-wide node space (a
    /// multiple of `1 << NODE_PAGE_BITS`); see [`crate::value::GlobalNode`].
    pub node_base: u32,
    /// Start of this file's range in the evaluator's position space; a
    /// node's `Pos` offset plus this is its [`crate::value::SrcPos`].
    pub base: u32,
    pub root: NodeId,
    pub nodes: &'a [Node],
    /// A pre-built cell for each literal node (`Int`, `Float`, `Str`,
    /// `Path`), `None` for other nodes.
    pub consts: &'a [Option<ValueRef<'a>>],
    pub strings: &'a [&'a [u8]],
    pub attr_sets: &'a [AttrSet<'a>],
    pub lambdas: &'a [Lambda<'a>],
    pub attr_names: &'a [AttrName],
    pub node_lists: &'a [NodeId],
    pub parts: &'a [(Pos, NodeId)],
    /// The compiled closure of each node, built the first time the node is
    /// evaluated (see `crate::closure`).
    pub closures: crate::closure::ClosureTable<'a>,
}

impl<'a> LinkedCode<'a> {
    pub fn node(&self, id: NodeId) -> &'a Node {
        &self.nodes[id.0 as usize]
    }

    pub fn attr_set(&self, id: AttrsId) -> &'a AttrSet<'a> {
        &self.attr_sets[id.0 as usize]
    }

    pub fn lambda(&self, id: LambdaId) -> &'a Lambda<'a> {
        &self.lambdas[id.0 as usize]
    }

    pub fn string(&self, id: LitId) -> &'a [u8] {
        self.strings[id.0 as usize]
    }

    pub fn nodes_in(&self, range: Range) -> &'a [NodeId] {
        &self.node_lists[range.start as usize..(range.start + range.len) as usize]
    }

    pub fn attr_names_in(&self, range: Range) -> &'a [AttrName] {
        &self.attr_names[range.start as usize..(range.start + range.len) as usize]
    }

    pub fn parts_in(&self, range: Range) -> &'a [(Pos, NodeId)] {
        &self.parts[range.start as usize..(range.start + range.len) as usize]
    }

    /// The position Nix's `Expr::getPos` reports for a node. A loop rather
    /// than a recursion: `!!…!x` may nest as deep as the source does.
    pub fn pos_of(&self, mut id: NodeId) -> Pos {
        loop {
            id = match *self.node(id) {
                Node::Var { pos, .. }
                | Node::InheritFrom { pos, .. }
                | Node::Select { pos, .. }
                | Node::Call { pos, .. }
                | Node::With { pos, .. }
                | Node::If { pos, .. }
                | Node::Assert { pos, .. }
                | Node::BinOp { pos, .. }
                | Node::ConcatStrings { pos, .. }
                | Node::CurPos(pos) => return pos,
                Node::Attrs(a) => return self.attr_set(a).pos,
                Node::Lambda(l) => return self.lambda(l).pos,
                Node::HasAttr { subject, .. } => subject,
                Node::Not(e) => e,
                Node::List { elems } => match self.nodes_in(elems).first() {
                    Some(&e) => e,
                    None => return Pos::NONE,
                },
                Node::Int(_)
                | Node::Float(_)
                | Node::Str(_)
                | Node::Path { .. }
                | Node::Let { .. } => return Pos::NONE,
            }
        }
    }

    /// Moves `code` into the arena. `consts` must be built by the caller
    /// (it needs the evaluator's value constructors) and have one entry per
    /// node.
    pub(crate) fn link(
        bump: &'a Bump,
        id: CodeId,
        base: u32,
        node_base: u32,
        code: &Code,
        consts: &'a [Option<ValueRef<'a>>],
    ) -> &'a LinkedCode<'a> {
        // The symbol-id permutation. The compiler sorted the definitions
        // by this evaluator's own ids, so it is the identity here; modules
        // from an image get theirs from `image::codec`, which links them.
        let id_order = |syms: &[Symbol]| -> &'a [u32] {
            let mut order: Vec<u32> = (0..syms.len() as u32).collect();
            order.sort_by_key(|&x| syms[x as usize]);
            bump.alloc_slice_copy(&order)
        };
        let strings: Vec<&'a [u8]> = code
            .strings
            .iter()
            .map(|s| &*bump.alloc_slice_copy(s))
            .collect();
        let attr_sets: Vec<AttrSet<'a>> = code
            .attr_sets
            .iter()
            .map(|s| {
                let names: Vec<Symbol> = s.attrs.iter().map(|a| a.name).collect();
                AttrSet {
                    recursive: s.recursive,
                    pos: s.pos,
                    attrs: bump.alloc_slice_copy(&s.attrs),
                    id_order: id_order(&names),
                    dynamic_attrs: bump.alloc_slice_copy(&s.dynamic_attrs),
                    inherit_from: bump.alloc_slice_copy(&s.inherit_from),
                }
            })
            .collect();
        let lambdas: Vec<Lambda<'a>> = code
            .lambdas
            .iter()
            .map(|l| Lambda {
                pos: l.pos,
                name: l.name,
                arg: l.arg,
                formals: l.formals.as_ref().map(|f| {
                    let names: Vec<Symbol> = f.formals.iter().map(|x| x.name).collect();
                    Formals {
                        formals: bump.alloc_slice_copy(&f.formals),
                        id_order: id_order(&names),
                        ellipsis: f.ellipsis,
                    }
                }),
                body: l.body,
            })
            .collect();
        bump.alloc(LinkedCode {
            id,
            node_base,
            base,
            root: code.root,
            nodes: bump.alloc_slice_copy(&code.nodes),
            consts,
            strings: bump.alloc_slice_copy(&strings),
            attr_sets: bump.alloc_slice_fill_iter(attr_sets),
            lambdas: bump.alloc_slice_fill_iter(lambdas),
            attr_names: bump.alloc_slice_copy(&code.attr_names),
            node_lists: bump.alloc_slice_copy(&code.node_lists),
            parts: bump.alloc_slice_copy(&code.parts),
            closures: crate::closure::ClosureTable::new(bump, code.nodes.len()),
        })
    }
}
