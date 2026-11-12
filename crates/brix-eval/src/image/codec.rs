//! The fixed-width words a module's compiled code is stored as, and
//! linking them straight into the evaluator's arena.
//!
//! [`crate::ir::Code`] is a handful of flat tables of small integers:
//! node ids, positions, symbols, ranges. A module is stored as those
//! tables, each entry a fixed number of little-endian 32-bit words, in
//! the image's code area (4-byte aligned), with its string literals as
//! one byte string beside them. Linking reads the words in place from
//! the mapped file and writes the evaluator's tables
//! ([`crate::code::LinkedCode`]) directly into the arena: one pass per
//! table of plain loads, range checks and stores, the string bytes
//! copied with one `memcpy`, and symbols remapped through the module's
//! interned name list. There is no varint, delta or decompression step
//! and no intermediate `Code`. (The evaluator's [`Node`] is a Rust enum
//! whose layout is not fixed, so it cannot be used from the file or
//! copied out of it without `unsafe`; every node must be validated
//! anyway, and writing it out while doing so costs little more.)
//!
//! The words are untrusted input like the rest of the image, and linking
//! validates everything it uses: every table index and range against the
//! table, every node reference against the node count, every position
//! against the source length, every tag and flag, every symbol against
//! the module's name list; a `with` variable must refer to an enclosing
//! (earlier) `with` node, so lookups follow a finite chain. Counts that
//! size an allocation are bounded by the words actually present, so a
//! damaged header cannot make the loader allocate more than a small
//! multiple of the image. What is not re-derived is static scoping: a
//! damaged environment level or displacement is caught by the
//! evaluator's bounds checks (a panic, not undefined behaviour), as
//! before.
//!
//! # Layout
//!
//! A module's words are a header of [`HEADER_WORDS`] counts followed by
//! the sections below, in this order; every section's length follows
//! from the header, and the module's words end exactly where the last
//! section does. "sym" is a compiling-table symbol id: below the image's
//! base symbol count it is the evaluator's own, above it an index into
//! the module's name list (see [`super::format`]). An optional node or
//! symbol is stored as `id + 1`, with 0 for none.
//!
//! ```text
//! new_names      1 word each   indices into the image name table, in
//!                              the order the compiler created them
//! string_ends    1 word each   end offset of each literal in the
//!                              module's string bytes
//! nodes          variable      one record per node, see below
//! sets           5 words each  flags (1 rec, 2 explicit displacements),
//!                              pos, #attrs, #dynamic, #inherit-from
//! defs           3 words each  sym | kind << 30, pos, expr
//! displs         1 word each   displacements of sets that spell them out
//! dynamic        3 words each  name node, value node, pos
//! inherit_from   1 word each   node
//! lambdas        6 words each  pos, name sym + 1, arg sym + 1, body,
//!                              formals flag (0 none, 1, 2 with `...`),
//!                              #formals
//! formals        3 words each  pos, sym, default node + 1
//! attr_names     1 word each   sym << 1 or node << 1 | 1
//! node_lists     1 word each   node
//! parts          2 words each  pos, node
//! ```
//!
//! A node record starts with `tag | small << 8`, where `small` is a
//! 24-bit field for values that are small by nature (an environment
//! level, an attribute path's length); a module whose values do not fit
//! is not stored as code and is parsed from source instead. The words
//! that follow are fixed by the tag (see `tag`). Records vary in length,
//! which keeps the common nodes (variables, selections, calls) at three
//! or four words; they are only ever read in order.
//!
//! Path literals relative to the source are compiled under
//! [`ROOT_MARKER`] and stored without it; linking puts the mount point in
//! front.

use crate::runtime::Bump;
use brix_syntax::ast::{BinOp, PathAnchor, Pos};
use rkyv::Archived;

use super::format::ROOT_MARKER;
use super::ImageError;
use crate::code::{AttrSet, Formals, Lambda};
use crate::ir::{
    AttrDef, AttrKind, AttrName, AttrsId, Code, DynamicAttrDef, Formal, LambdaId, LitId, Node,
    NodeId, Range, VarRef,
};
use crate::symbol::Symbol;

/// A word as it lies in the image: a little-endian `u32`, 4-byte aligned.
pub type Word = Archived<u32>;

/// Node tags. The numbers are part of the format; the comment lists the
/// words after the first.
mod tag {
    /// lo, hi
    pub const INT: u8 = 0;
    /// lo, hi (the bits)
    pub const FLOAT: u8 = 1;
    /// lit
    pub const STR: u8 = 2;
    /// lit
    pub const PATH_ROOT: u8 = 3;
    /// lit (stored without the marker root)
    pub const PATH_SOURCE: u8 = 4;
    /// small = level; pos, sym, displ
    pub const VAR_ENV: u8 = 5;
    /// small = level; pos, sym, with node
    pub const VAR_WITH: u8 = 6;
    /// pos, displ
    pub const INHERIT_FROM: u8 = 7;
    /// small = path length; pos, subject, path start
    pub const SELECT: u8 = 8;
    /// small = path length; pos, subject, path start, default
    pub const SELECT_DEFAULT: u8 = 9;
    /// small = path length; subject, path start
    pub const HAS_ATTR: u8 = 10;
    /// set
    pub const ATTRS: u8 = 11;
    /// start, len
    pub const LIST: u8 = 12;
    /// lambda
    pub const LAMBDA: u8 = 13;
    /// small = argument count; pos, fun, args start
    pub const CALL: u8 = 14;
    /// set, body
    pub const LET: u8 = 15;
    /// small = prev_with; pos, attrs, body, parent_with + 1
    pub const WITH: u8 = 16;
    /// pos, cond, then, else
    pub const IF: u8 = 17;
    /// pos, cond, body
    pub const ASSERT: u8 = 18;
    /// operand
    pub const NOT: u8 = 19;
    /// small = operator; pos, lhs, rhs
    pub const BIN_OP: u8 = 20;
    /// small = force_string; pos, parts start, parts len
    pub const CONCAT_STRINGS: u8 = 21;
    /// pos
    pub const CUR_POS: u8 = 22;
}

/// Words in a module's header; see the module docs.
pub const HEADER_WORDS: usize = 15;

/// Header word indices.
mod h {
    pub const NEW_NAMES: usize = 0;
    pub const STRINGS: usize = 1;
    pub const NODES: usize = 2;
    pub const NODE_WORDS: usize = 3;
    pub const ROOT: usize = 4;
    pub const SETS: usize = 5;
    pub const DEFS: usize = 6;
    pub const DISPLS: usize = 7;
    pub const DYNAMIC: usize = 8;
    pub const INHERIT_FROM: usize = 9;
    pub const LAMBDAS: usize = 10;
    pub const FORMALS: usize = 11;
    pub const ATTR_NAMES: usize = 12;
    pub const NODE_LISTS: usize = 13;
    pub const PARTS: usize = 14;
}

/// The sections after the header, in order: name, the header word that
/// counts its entries, and words per entry (0: the count is the length
/// in words).
const SECTIONS: [(&str, usize, usize); 13] = [
    ("new_names", h::NEW_NAMES, 1),
    ("string_ends", h::STRINGS, 1),
    ("nodes", h::NODE_WORDS, 0),
    ("sets", h::SETS, 5),
    ("defs", h::DEFS, 3),
    ("displs", h::DISPLS, 1),
    ("dynamic", h::DYNAMIC, 3),
    ("inherit_from", h::INHERIT_FROM, 1),
    ("lambdas", h::LAMBDAS, 6),
    ("formals", h::FORMALS, 3),
    ("attr_names", h::ATTR_NAMES, 1),
    ("node_lists", h::NODE_LISTS, 1),
    ("parts", h::PARTS, 2),
];

/// The largest value a node record's `small` field holds.
const SMALL_MAX: u32 = (1 << 24) - 1;
/// Symbols share their word with an attribute kind (two bits) or an
/// attribute-name tag (one bit).
const SYM_LIMIT: u32 = 1 << 30;

fn bin_op_code(op: BinOp) -> u32 {
    match op {
        BinOp::Eq => 0,
        BinOp::NEq => 1,
        BinOp::And => 2,
        BinOp::Or => 3,
        BinOp::Impl => 4,
        BinOp::Update => 5,
        BinOp::ConcatLists => 6,
    }
}

fn bin_op_from_code(code: u32) -> Result<BinOp, Bad> {
    Ok(match code {
        0 => BinOp::Eq,
        1 => BinOp::NEq,
        2 => BinOp::And,
        3 => BinOp::Or,
        4 => BinOp::Impl,
        5 => BinOp::Update,
        6 => BinOp::ConcatLists,
        _ => return Err(bad("unknown binary operator")),
    })
}

fn kind_code(kind: AttrKind) -> u32 {
    match kind {
        AttrKind::Plain => 0,
        AttrKind::Inherited => 1,
        AttrKind::InheritedFrom => 2,
    }
}

// ---------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------

/// A module in image form: its words and its string bytes.
#[derive(Debug, Default)]
pub(crate) struct Encoded {
    pub words: Vec<u32>,
    pub strings: Vec<u8>,
}

/// Why a module cannot be stored as code (it is then parsed from source).
#[derive(Debug)]
pub(crate) struct Unencodable(pub &'static str);

/// Where a value would not fit its field.
fn fits(v: u32, limit: u32, what: &'static str) -> Result<u32, Unencodable> {
    if v <= limit {
        Ok(v)
    } else {
        Err(Unencodable(what))
    }
}

/// Encodes `code`, whose symbols are the compiling table's ids, with
/// `new_names` the image name-table indices of the names that table
/// created beyond the base.
pub(crate) fn encode(code: &Code, new_names: &[u32]) -> Result<Encoded, Unencodable> {
    let n_nodes = u32::try_from(code.nodes.len()).map_err(|_| Unencodable("too many nodes"))?;
    if n_nodes >= 1 << 31 {
        return Err(Unencodable("too many nodes"));
    }
    let sym = |s: Symbol| fits(s.0, SYM_LIMIT - 1, "too many symbols");
    let opt_sym = |s: Option<Symbol>| -> Result<u32, Unencodable> {
        Ok(match s {
            None => 0,
            Some(s) => sym(s)? + 1,
        })
    };
    let opt_node = |n: Option<NodeId>| n.map_or(0, |n| n.0 + 1);
    let small = |v: u32| fits(v, SMALL_MAX, "a node field outgrew 24 bits");

    let mut words: Vec<u32> = vec![0; HEADER_WORDS];
    let mut header = [0u32; HEADER_WORDS];

    header[h::NEW_NAMES] = new_names.len() as u32;
    words.extend_from_slice(new_names);

    // String literals. A literal referenced by a source-relative path
    // node is stored without the marker root.
    let mut source_path = vec![false; code.strings.len()];
    for node in &code.nodes {
        if let Node::Path {
            anchor: PathAnchor::Source,
            lit,
        } = node
        {
            source_path[lit.0 as usize] = true;
        }
    }
    let mut strings = Vec::new();
    header[h::STRINGS] = code.strings.len() as u32;
    for (i, s) in code.strings.iter().enumerate() {
        let stored: &[u8] = if source_path[i] {
            let marker = ROOT_MARKER.as_bytes();
            match s.strip_prefix(marker) {
                Some(rest) if rest.is_empty() || rest[0] == b'/' => rest,
                _ => return Err(Unencodable("a path literal outside the tree")),
            }
        } else {
            s
        };
        strings.extend_from_slice(stored);
        words.push(
            u32::try_from(strings.len()).map_err(|_| Unencodable("more than 4 GiB of strings"))?,
        );
    }
    // A literal shared between a path and a string node (the compiler
    // never does that) would need two stored forms.
    for node in &code.nodes {
        if let Node::Str(lit)
        | Node::Path {
            anchor: PathAnchor::Root,
            lit,
        } = node
        {
            if source_path[lit.0 as usize] {
                return Err(Unencodable("a literal shared by a path and a string"));
            }
        }
    }

    header[h::NODES] = n_nodes;
    header[h::ROOT] = code.root.0;
    let nodes_start = words.len();
    for node in &code.nodes {
        let t = |tag: u8, small: u32| u32::from(tag) | (small << 8);
        match *node {
            Node::Int(v) => {
                let v = v as u64;
                words.extend_from_slice(&[t(tag::INT, 0), v as u32, (v >> 32) as u32]);
            }
            Node::Float(f) => {
                let v = f.to_bits();
                words.extend_from_slice(&[t(tag::FLOAT, 0), v as u32, (v >> 32) as u32]);
            }
            Node::Str(lit) => words.extend_from_slice(&[t(tag::STR, 0), lit.0]),
            Node::Path { anchor, lit } => {
                let tg = match anchor {
                    PathAnchor::Root => tag::PATH_ROOT,
                    PathAnchor::Source => tag::PATH_SOURCE,
                };
                words.extend_from_slice(&[t(tg, 0), lit.0]);
            }
            Node::Var { pos, name, var } => match var {
                VarRef::Env { level, displ } => words.extend_from_slice(&[
                    t(tag::VAR_ENV, small(level)?),
                    pos.0,
                    sym(name)?,
                    displ,
                ]),
                VarRef::With { level, with } => words.extend_from_slice(&[
                    t(tag::VAR_WITH, small(level)?),
                    pos.0,
                    sym(name)?,
                    with.0,
                ]),
            },
            Node::InheritFrom { pos, displ } => {
                words.extend_from_slice(&[t(tag::INHERIT_FROM, 0), pos.0, displ])
            }
            Node::Select {
                pos,
                subject,
                path,
                default,
            } => {
                let len = small(path.len)?;
                match default {
                    None => words.extend_from_slice(&[
                        t(tag::SELECT, len),
                        pos.0,
                        subject.0,
                        path.start,
                    ]),
                    Some(d) => words.extend_from_slice(&[
                        t(tag::SELECT_DEFAULT, len),
                        pos.0,
                        subject.0,
                        path.start,
                        d.0,
                    ]),
                }
            }
            Node::HasAttr { subject, path } => words.extend_from_slice(&[
                t(tag::HAS_ATTR, small(path.len)?),
                subject.0,
                path.start,
            ]),
            Node::Attrs(a) => words.extend_from_slice(&[t(tag::ATTRS, 0), a.0]),
            Node::List { elems } => {
                words.extend_from_slice(&[t(tag::LIST, 0), elems.start, elems.len])
            }
            Node::Lambda(l) => words.extend_from_slice(&[t(tag::LAMBDA, 0), l.0]),
            Node::Call { pos, fun, args } => {
                words.extend_from_slice(&[t(tag::CALL, small(args.len)?), pos.0, fun.0, args.start])
            }
            Node::Let { attrs, body } => {
                words.extend_from_slice(&[t(tag::LET, 0), attrs.0, body.0])
            }
            Node::With {
                pos,
                attrs,
                body,
                prev_with,
                parent_with,
            } => words.extend_from_slice(&[
                t(tag::WITH, small(prev_with)?),
                pos.0,
                attrs.0,
                body.0,
                opt_node(parent_with),
            ]),
            Node::If {
                pos,
                cond,
                then_,
                else_,
            } => words.extend_from_slice(&[t(tag::IF, 0), pos.0, cond.0, then_.0, else_.0]),
            Node::Assert { pos, cond, body } => {
                words.extend_from_slice(&[t(tag::ASSERT, 0), pos.0, cond.0, body.0])
            }
            Node::Not(e) => words.extend_from_slice(&[t(tag::NOT, 0), e.0]),
            Node::BinOp { pos, op, lhs, rhs } => {
                words.extend_from_slice(&[t(tag::BIN_OP, bin_op_code(op)), pos.0, lhs.0, rhs.0])
            }
            Node::ConcatStrings {
                pos,
                force_string,
                parts,
            } => words.extend_from_slice(&[
                t(tag::CONCAT_STRINGS, u32::from(force_string)),
                pos.0,
                parts.start,
                parts.len,
            ]),
            Node::CurPos(pos) => words.extend_from_slice(&[t(tag::CUR_POS, 0), pos.0]),
        }
    }
    header[h::NODE_WORDS] = (words.len() - nodes_start) as u32;

    header[h::SETS] = code.attr_sets.len() as u32;
    let (mut defs, mut displs, mut dynamic, mut inherit_from) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for set in &code.attr_sets {
        // The displacement of a static attribute is its index (the
        // compiler assigns them in order); only an unusual set spells
        // them out.
        let explicit = set
            .attrs
            .iter()
            .enumerate()
            .any(|(i, a)| a.displ as usize != i);
        words.extend_from_slice(&[
            u32::from(set.recursive) | (u32::from(explicit) << 1),
            set.pos.0,
            set.attrs.len() as u32,
            set.dynamic_attrs.len() as u32,
            set.inherit_from.len() as u32,
        ]);
        for a in &set.attrs {
            defs.extend_from_slice(&[sym(a.name)? | (kind_code(a.kind) << 30), a.pos.0, a.expr.0]);
            if explicit {
                displs.push(a.displ);
            }
        }
        for d in &set.dynamic_attrs {
            dynamic.extend_from_slice(&[d.name.0, d.value.0, d.pos.0]);
        }
        inherit_from.extend(set.inherit_from.iter().map(|n| n.0));
    }
    header[h::DEFS] = (defs.len() / 3) as u32;
    header[h::DISPLS] = displs.len() as u32;
    header[h::DYNAMIC] = (dynamic.len() / 3) as u32;
    header[h::INHERIT_FROM] = inherit_from.len() as u32;
    words.extend(defs);
    words.extend(displs);
    words.extend(dynamic);
    words.extend(inherit_from);

    header[h::LAMBDAS] = code.lambdas.len() as u32;
    let mut formals = Vec::new();
    for l in &code.lambdas {
        let (flag, n) = match &l.formals {
            None => (0, 0),
            Some(f) => (1 + u32::from(f.ellipsis), f.formals.len() as u32),
        };
        words.extend_from_slice(&[
            l.pos.0,
            opt_sym(l.name)?,
            opt_sym(l.arg)?,
            l.body.0,
            flag,
            n,
        ]);
        if let Some(f) = &l.formals {
            for x in &f.formals {
                formals.extend_from_slice(&[x.pos.0, sym(x.name)?, opt_node(x.default)]);
            }
        }
    }
    header[h::FORMALS] = (formals.len() / 3) as u32;
    words.extend(formals);

    header[h::ATTR_NAMES] = code.attr_names.len() as u32;
    for n in &code.attr_names {
        words.push(match *n {
            AttrName::Static(s) => sym(s)? << 1,
            AttrName::Dynamic(node) => (node.0 << 1) | 1,
        });
    }
    header[h::NODE_LISTS] = code.node_lists.len() as u32;
    words.extend(code.node_lists.iter().map(|n| n.0));
    header[h::PARTS] = code.parts.len() as u32;
    for (p, n) in &code.parts {
        words.extend_from_slice(&[p.0, n.0]);
    }
    words[..HEADER_WORDS].copy_from_slice(&header);
    Ok(Encoded { words, strings })
}

// ---------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------

/// What is wrong with a damaged module; turned into a message once,
/// outside the hot loops.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bad(&'static str);

#[cold]
#[inline(never)]
fn bad(what: &'static str) -> Bad {
    Bad(what)
}

impl std::fmt::Display for Bad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "module code: {}", self.0)
    }
}

/// The section lengths, in words, that a header declares; `None` when
/// they overflow.
fn section_lengths(header: &[u32; HEADER_WORDS]) -> Option<[usize; SECTIONS.len()]> {
    let mut out = [0usize; SECTIONS.len()];
    for (i, &(_, count, width)) in SECTIONS.iter().enumerate() {
        let n = header[count] as usize;
        out[i] = if width == 0 { n } else { n.checked_mul(width)? };
    }
    Some(out)
}

/// How many words each section of a module takes, for size analysis
/// (the `image_stats` example).
#[doc(hidden)]
pub fn section_sizes(words: &[u32]) -> Result<Vec<(&'static str, usize)>, ImageError> {
    let header: [u32; HEADER_WORDS] = words
        .get(..HEADER_WORDS)
        .ok_or_else(|| ImageError::Corrupt("module code: no header".into()))?
        .try_into()
        .expect("header words");
    let lens = section_lengths(&header)
        .ok_or_else(|| ImageError::Corrupt("module code: section too long".into()))?;
    let mut out = vec![("header", HEADER_WORDS)];
    out.extend(SECTIONS.iter().zip(lens).map(|(s, n)| (s.0, n)));
    Ok(out)
}

/// A module's words with the header read and the sections split, still
/// in place in the image.
pub(crate) struct ModuleCode<'i> {
    header: [u32; HEADER_WORDS],
    sections: [&'i [Word]; SECTIONS.len()],
    strings: &'i [u8],
}

/// Section indices into [`ModuleCode::sections`], in [`SECTIONS`] order.
mod s {
    pub const NEW_NAMES: usize = 0;
    pub const STRING_ENDS: usize = 1;
    pub const NODES: usize = 2;
    pub const SETS: usize = 3;
    pub const DEFS: usize = 4;
    pub const DISPLS: usize = 5;
    pub const DYNAMIC: usize = 6;
    pub const INHERIT_FROM: usize = 7;
    pub const LAMBDAS: usize = 8;
    pub const FORMALS: usize = 9;
    pub const ATTR_NAMES: usize = 10;
    pub const NODE_LISTS: usize = 11;
    pub const PARTS: usize = 12;
}

/// The tables of a linked module, in the arena. Together with the
/// literal cells (which need the evaluator) this is a
/// [`crate::code::LinkedCode`].
pub(crate) struct Linked<'a> {
    pub root: NodeId,
    pub nodes: &'a [Node],
    pub strings: &'a [&'a [u8]],
    pub attr_sets: &'a [AttrSet<'a>],
    pub lambdas: &'a [Lambda<'a>],
    pub attr_names: &'a [AttrName],
    pub node_lists: &'a [NodeId],
    pub parts: &'a [(Pos, NodeId)],
}

/// Reads `K` words, advancing `rest`.
#[inline(always)]
fn take<const K: usize>(rest: &mut &[Word]) -> Result<[u32; K], Bad> {
    let Some((head, tail)) = rest.split_first_chunk::<K>() else {
        return Err(bad("truncated table"));
    };
    *rest = tail;
    Ok(std::array::from_fn(|i| head[i].to_native()))
}

/// Allocates `n` values in the arena, the `i`-th being `f(i)`. The first
/// error stops the work (the remaining slots get `filler()`) and is
/// returned.
fn try_fill<T>(
    bump: &Bump,
    n: usize,
    filler: impl Fn() -> T,
    mut f: impl FnMut(usize) -> Result<T, Bad>,
) -> Result<&mut [T], Bad> {
    if n == 0 {
        // Most sets have no dynamic attributes and no `inherit (…)`.
        return Ok(&mut []);
    }
    let mut err = None;
    let out = bump.alloc_slice_fill_with(n, |i| {
        if err.is_some() {
            return filler();
        }
        match f(i) {
            Ok(v) => v,
            Err(e) => {
                err = Some(e);
                filler()
            }
        }
    });
    match err {
        None => Ok(out),
        Some(e) => Err(e),
    }
}

/// Everything a reference is checked against.
struct Check<'s> {
    nodes: u32,
    strings: u32,
    sets: u32,
    lambdas: u32,
    attr_names: u32,
    node_lists: u32,
    parts: u32,
    source_len: u32,
    base: u32,
    syms: &'s [Symbol],
}

impl Check<'_> {
    #[inline(always)]
    fn node(&self, v: u32) -> Result<NodeId, Bad> {
        if v < self.nodes {
            Ok(NodeId(v))
        } else {
            Err(bad("node reference out of range"))
        }
    }

    #[inline(always)]
    fn opt_node(&self, v: u32) -> Result<Option<NodeId>, Bad> {
        match v {
            0 => Ok(None),
            v => self.node(v - 1).map(Some),
        }
    }

    #[inline(always)]
    fn pos(&self, v: u32) -> Result<Pos, Bad> {
        // The end of the file is a position too (`register_source`).
        if v <= self.source_len || v == Pos::NONE.0 {
            Ok(Pos(v))
        } else {
            Err(bad("position out of range"))
        }
    }

    #[inline(always)]
    fn sym(&self, v: u32) -> Result<Symbol, Bad> {
        if v < self.base {
            return Ok(Symbol(v));
        }
        match self.syms.get((v - self.base) as usize) {
            Some(&s) => Ok(s),
            None => Err(bad("symbol out of range")),
        }
    }

    #[inline(always)]
    fn opt_sym(&self, v: u32) -> Result<Option<Symbol>, Bad> {
        match v {
            0 => Ok(None),
            v => self.sym(v - 1).map(Some),
        }
    }

    #[inline(always)]
    fn lit(&self, v: u32) -> Result<LitId, Bad> {
        if v < self.strings {
            Ok(LitId(v))
        } else {
            Err(bad("string index out of range"))
        }
    }

    #[inline(always)]
    fn range(&self, start: u32, len: u32, table: u32) -> Result<Range, Bad> {
        if u64::from(start) + u64::from(len) <= u64::from(table) {
            Ok(Range { start, len })
        } else {
            Err(bad("range exceeds its table"))
        }
    }

    #[inline(always)]
    fn index(&self, v: u32, len: u32) -> Result<u32, Bad> {
        if v < len {
            Ok(v)
        } else {
            Err(bad("table index out of range"))
        }
    }
}

/// Entries `at..at + len` of a section of `width`-word entries.
fn span(of: &[Word], at: usize, len: usize, width: usize) -> Result<&[Word], Bad> {
    of.get(at * width..(at + len) * width)
        .ok_or_else(|| bad("a table range exceeds its section"))
}

#[inline(always)]
fn no_small(small: u32) -> Result<(), Bad> {
    if small == 0 {
        Ok(())
    } else {
        Err(bad("unexpected flags in a node"))
    }
}

/// `0, 1, 2, …`: the order of most small sets and formal lists, shared
/// rather than allocated for each.
static IDENTITY: [u32; 256] = {
    let mut a = [0u32; 256];
    let mut i = 0;
    while i < a.len() {
        a[i] = i as u32;
        i += 1;
    }
    a
};

/// The identity permutation of `n` elements.
fn identity(bump: &Bump, n: usize) -> &[u32] {
    match IDENTITY.get(..n) {
        Some(order) => order,
        None => bump.alloc_slice_fill_with(n, |i| i as u32),
    }
}

/// Indices `0..n` in increasing symbol-id order of `name(i)`: the order
/// Nix visits bindings in. The identity when the names are already in
/// that order (always so for a module whose names were all new). The
/// names must be distinct, as the compiler leaves them; a repeated one
/// is damage.
fn id_order(bump: &Bump, n: usize, name: impl Fn(usize) -> Symbol) -> Result<&[u32], Bad> {
    if (1..n).all(|i| name(i - 1) < name(i)) {
        return Ok(identity(bump, n));
    }
    let order = bump.alloc_slice_fill_with(n, |i| i as u32);
    order.sort_unstable_by_key(|&i| name(i as usize));
    if order
        .windows(2)
        .any(|w| name(w[0] as usize) == name(w[1] as usize))
    {
        return Err(bad("repeated name"));
    }
    Ok(order)
}

impl<'i> ModuleCode<'i> {
    /// Splits a module's words into its sections, checking that they add
    /// up. `strings` are the module's string bytes.
    pub(crate) fn parse(words: &'i [Word], strings: &'i [u8]) -> Result<ModuleCode<'i>, Bad> {
        let Some((head, mut rest)) = words.split_first_chunk::<HEADER_WORDS>() else {
            return Err(bad("no header"));
        };
        let header: [u32; HEADER_WORDS] = std::array::from_fn(|i| head[i].to_native());
        let lens = section_lengths(&header).ok_or_else(|| bad("section too long"))?;
        let mut sections: [&'i [Word]; SECTIONS.len()] = [&[]; SECTIONS.len()];
        for (slot, len) in sections.iter_mut().zip(lens) {
            let Some((section, tail)) = rest.split_at_checked(len) else {
                return Err(bad("sections exceed the module"));
            };
            *slot = section;
            rest = tail;
        }
        if !rest.is_empty() {
            return Err(bad("trailing words"));
        }
        // Every count that sizes an allocation is bounded by the words
        // present: nodes take at least two words each.
        let n_nodes = header[h::NODES];
        if n_nodes == 0
            || u64::from(n_nodes) * 2 > u64::from(header[h::NODE_WORDS])
            || header[h::ROOT] >= n_nodes
        {
            return Err(bad("bad node count or root"));
        }
        Ok(ModuleCode {
            header,
            sections,
            strings,
        })
    }

    /// The module's new names, as indices into the image's name table,
    /// in the order they must be interned.
    pub(crate) fn new_names(&self) -> impl ExactSizeIterator<Item = u32> + 'i {
        self.sections[s::NEW_NAMES].iter().map(|w| w.to_native())
    }

    /// Writes the module's tables into the arena. `syms` are the
    /// evaluator's symbols for [`Self::new_names`], `base` the image's
    /// base symbol count, `source_len` the module's source length and
    /// `mount_abs` the mount point that replaces the marker root in path
    /// literals (empty for `/`). Paths are bytes, as in Nix: a path
    /// literal is the mount point's bytes followed by the literal's.
    pub(crate) fn link<'a>(
        &self,
        bump: &'a Bump,
        base: u32,
        syms: &[Symbol],
        source_len: u32,
        mount_abs: &[u8],
    ) -> Result<Linked<'a>, Bad> {
        let hd = &self.header;
        let sec = &self.sections;
        if syms.len() != sec[s::NEW_NAMES].len() {
            return Err(bad("symbol list does not match the module"));
        }
        let check = Check {
            nodes: hd[h::NODES],
            strings: hd[h::STRINGS],
            sets: hd[h::SETS],
            lambdas: hd[h::LAMBDAS],
            attr_names: hd[h::ATTR_NAMES],
            node_lists: hd[h::NODE_LISTS],
            parts: hd[h::PARTS],
            source_len,
            base,
            syms,
        };

        // Strings: the bytes in one copy, each literal a slice of it.
        let all: &'a [u8] = bump.alloc_slice_copy(self.strings);
        let mut prev = 0usize;
        let ends = sec[s::STRING_ENDS];
        let strings: &'a mut [&'a [u8]] = try_fill(
            bump,
            ends.len(),
            || &[][..],
            |i| {
                let end = ends[i].to_native() as usize;
                let s = all
                    .get(prev..end)
                    .ok_or_else(|| bad("string out of range"))?;
                prev = end;
                Ok(s)
            },
        )?;

        let nodes = self.link_nodes(bump, &check, strings, mount_abs)?;
        let attr_sets = self.link_sets(bump, &check)?;
        let lambdas = self.link_lambdas(bump, &check)?;

        let names = sec[s::ATTR_NAMES];
        let attr_names = try_fill(
            bump,
            names.len(),
            || AttrName::Dynamic(NodeId(0)),
            |i| {
                let v = names[i].to_native();
                if v & 1 == 0 {
                    check.sym(v >> 1).map(AttrName::Static)
                } else {
                    check.node(v >> 1).map(AttrName::Dynamic)
                }
            },
        )?;
        let lists = sec[s::NODE_LISTS];
        let node_lists = try_fill(
            bump,
            lists.len(),
            || NodeId(0),
            |i| check.node(lists[i].to_native()),
        )?;
        let parts_w = sec[s::PARTS];
        let parts = try_fill(
            bump,
            parts_w.len() / 2,
            || (Pos::NONE, NodeId(0)),
            |i| {
                let (p, n) = (parts_w[2 * i].to_native(), parts_w[2 * i + 1].to_native());
                Ok((check.pos(p)?, check.node(n)?))
            },
        )?;
        Ok(Linked {
            root: NodeId(hd[h::ROOT]),
            nodes,
            strings,
            attr_sets,
            lambdas,
            attr_names,
            node_lists,
            parts,
        })
    }

    fn link_nodes<'a>(
        &self,
        bump: &'a Bump,
        check: &Check<'_>,
        strings: &mut [&'a [u8]],
        mount_abs: &[u8],
    ) -> Result<&'a [Node], Bad> {
        let mut rest = self.sections[s::NODES];
        // Which nodes are `with`s: a `with` variable and a `with`'s
        // parent must name an earlier one.
        let mut withs = vec![0u64; (check.nodes as usize).div_ceil(64)];
        let is_with = |withs: &[u64], at: u32, target: NodeId| {
            target.0 < at && withs[target.0 as usize / 64] & (1 << (target.0 % 64)) != 0
        };
        let nodes = try_fill(
            bump,
            check.nodes as usize,
            || Node::Int(0),
            |i| {
                let at = i as u32;
                let Some(first) = rest.first() else {
                    return Err(bad("truncated node table"));
                };
                let w0 = first.to_native();
                let small = w0 >> 8;
                Ok(match w0 as u8 {
                    tag::INT => {
                        no_small(small)?;
                        let [_, lo, hi] = take(&mut rest)?;
                        Node::Int(((u64::from(hi) << 32) | u64::from(lo)) as i64)
                    }
                    tag::FLOAT => {
                        no_small(small)?;
                        let [_, lo, hi] = take(&mut rest)?;
                        Node::Float(f64::from_bits((u64::from(hi) << 32) | u64::from(lo)))
                    }
                    tag::STR => {
                        no_small(small)?;
                        let [_, lit] = take(&mut rest)?;
                        Node::Str(check.lit(lit)?)
                    }
                    tag::PATH_ROOT => {
                        no_small(small)?;
                        let [_, lit] = take(&mut rest)?;
                        Node::Path {
                            anchor: PathAnchor::Root,
                            lit: check.lit(lit)?,
                        }
                    }
                    tag::PATH_SOURCE => {
                        no_small(small)?;
                        let [_, lit] = take(&mut rest)?;
                        let lit = check.lit(lit)?;
                        // `<mount>/rest`, and `/` for the root of a tree
                        // mounted at `/`.
                        let rel = strings[lit.0 as usize];
                        let path: &'a [u8] = if rel.is_empty() && mount_abs.is_empty() {
                            b"/"
                        } else {
                            let out = bump.alloc_slice_fill_copy(mount_abs.len() + rel.len(), 0u8);
                            out[..mount_abs.len()].copy_from_slice(mount_abs);
                            out[mount_abs.len()..].copy_from_slice(rel);
                            out
                        };
                        strings[lit.0 as usize] = path;
                        Node::Path {
                            anchor: PathAnchor::Source,
                            lit,
                        }
                    }
                    tag::VAR_ENV => {
                        let [_, pos, name, displ] = take(&mut rest)?;
                        Node::Var {
                            pos: check.pos(pos)?,
                            name: check.sym(name)?,
                            var: VarRef::Env {
                                level: small,
                                displ,
                            },
                        }
                    }
                    tag::VAR_WITH => {
                        let [_, pos, name, with] = take(&mut rest)?;
                        let with = check.node(with)?;
                        if !is_with(&withs, at, with) {
                            return Err(bad("with variable not under a with"));
                        }
                        Node::Var {
                            pos: check.pos(pos)?,
                            name: check.sym(name)?,
                            var: VarRef::With { level: small, with },
                        }
                    }
                    tag::INHERIT_FROM => {
                        no_small(small)?;
                        let [_, pos, displ] = take(&mut rest)?;
                        Node::InheritFrom {
                            pos: check.pos(pos)?,
                            displ,
                        }
                    }
                    tag::SELECT => {
                        let [_, pos, subject, start] = take(&mut rest)?;
                        Node::Select {
                            pos: check.pos(pos)?,
                            subject: check.node(subject)?,
                            path: check.range(start, small, check.attr_names)?,
                            default: None,
                        }
                    }
                    tag::SELECT_DEFAULT => {
                        let [_, pos, subject, start, default] = take(&mut rest)?;
                        Node::Select {
                            pos: check.pos(pos)?,
                            subject: check.node(subject)?,
                            path: check.range(start, small, check.attr_names)?,
                            default: Some(check.node(default)?),
                        }
                    }
                    tag::HAS_ATTR => {
                        let [_, subject, start] = take(&mut rest)?;
                        Node::HasAttr {
                            subject: check.node(subject)?,
                            path: check.range(start, small, check.attr_names)?,
                        }
                    }
                    tag::ATTRS => {
                        no_small(small)?;
                        let [_, set] = take(&mut rest)?;
                        Node::Attrs(AttrsId(check.index(set, check.sets)?))
                    }
                    tag::LIST => {
                        no_small(small)?;
                        let [_, start, len] = take(&mut rest)?;
                        Node::List {
                            elems: check.range(start, len, check.node_lists)?,
                        }
                    }
                    tag::LAMBDA => {
                        no_small(small)?;
                        let [_, l] = take(&mut rest)?;
                        Node::Lambda(LambdaId(check.index(l, check.lambdas)?))
                    }
                    tag::CALL => {
                        let [_, pos, fun, start] = take(&mut rest)?;
                        Node::Call {
                            pos: check.pos(pos)?,
                            fun: check.node(fun)?,
                            args: check.range(start, small, check.node_lists)?,
                        }
                    }
                    tag::LET => {
                        no_small(small)?;
                        let [_, set, body] = take(&mut rest)?;
                        Node::Let {
                            attrs: AttrsId(check.index(set, check.sets)?),
                            body: check.node(body)?,
                        }
                    }
                    tag::WITH => {
                        let [_, pos, attrs, body, parent] = take(&mut rest)?;
                        let parent_with = check.opt_node(parent)?;
                        if let Some(p) = parent_with {
                            if !is_with(&withs, at, p) {
                                return Err(bad("with parent is not an enclosing with"));
                            }
                        }
                        withs[i / 64] |= 1 << (i % 64);
                        Node::With {
                            pos: check.pos(pos)?,
                            attrs: check.node(attrs)?,
                            body: check.node(body)?,
                            prev_with: small,
                            parent_with,
                        }
                    }
                    tag::IF => {
                        no_small(small)?;
                        let [_, pos, cond, then_, else_] = take(&mut rest)?;
                        Node::If {
                            pos: check.pos(pos)?,
                            cond: check.node(cond)?,
                            then_: check.node(then_)?,
                            else_: check.node(else_)?,
                        }
                    }
                    tag::ASSERT => {
                        no_small(small)?;
                        let [_, pos, cond, body] = take(&mut rest)?;
                        Node::Assert {
                            pos: check.pos(pos)?,
                            cond: check.node(cond)?,
                            body: check.node(body)?,
                        }
                    }
                    tag::NOT => {
                        no_small(small)?;
                        let [_, e] = take(&mut rest)?;
                        Node::Not(check.node(e)?)
                    }
                    tag::BIN_OP => {
                        let [_, pos, lhs, rhs] = take(&mut rest)?;
                        Node::BinOp {
                            pos: check.pos(pos)?,
                            op: bin_op_from_code(small)?,
                            lhs: check.node(lhs)?,
                            rhs: check.node(rhs)?,
                        }
                    }
                    tag::CONCAT_STRINGS => {
                        let [_, pos, start, len] = take(&mut rest)?;
                        Node::ConcatStrings {
                            pos: check.pos(pos)?,
                            force_string: match small {
                                0 => false,
                                1 => true,
                                _ => return Err(bad("bad string flag")),
                            },
                            parts: check.range(start, len, check.parts)?,
                        }
                    }
                    tag::CUR_POS => {
                        no_small(small)?;
                        let [_, pos] = take(&mut rest)?;
                        Node::CurPos(check.pos(pos)?)
                    }
                    _ => return Err(bad("unknown node tag")),
                })
            },
        )?;
        if !rest.is_empty() {
            return Err(bad("trailing node words"));
        }
        Ok(nodes)
    }

    fn link_sets<'a>(&self, bump: &'a Bump, check: &Check<'_>) -> Result<&'a [AttrSet<'a>], Bad> {
        let sec = &self.sections;
        let (defs_w, displs_w, dyn_w, from_w) = (
            sec[s::DEFS],
            sec[s::DISPLS],
            sec[s::DYNAMIC],
            sec[s::INHERIT_FROM],
        );
        let mut records = sec[s::SETS];
        // Cursors into the per-entry sections.
        let (mut d, mut x, mut y, mut z) = (0usize, 0usize, 0usize, 0usize);
        let empty = || AttrSet {
            recursive: false,
            pos: Pos::NONE,
            attrs: &[],
            id_order: &[],
            dynamic_attrs: &[],
            inherit_from: &[],
        };
        let sets = try_fill(bump, check.sets as usize, empty, |_| {
            let [flags, pos, n, n_dyn, n_from] = take(&mut records)?;
            if flags > 3 {
                return Err(bad("bad attribute set flags"));
            }
            let explicit = flags & 2 != 0;
            let (n, n_dyn, n_from) = (n as usize, n_dyn as usize, n_from as usize);
            let defs = span(defs_w, d, n, 3)?;
            let displs = if explicit {
                span(displs_w, x, n, 1)?
            } else {
                &[]
            };
            let dynamic = span(dyn_w, y, n_dyn, 3)?;
            let from = span(from_w, z, n_from, 1)?;
            d += n;
            if explicit {
                x += n;
            }
            y += n_dyn;
            z += n_from;
            let attrs: &'a [AttrDef] = try_fill(
                bump,
                n,
                || AttrDef {
                    name: Symbol(0),
                    pos: Pos::NONE,
                    kind: AttrKind::Plain,
                    expr: NodeId(0),
                    displ: 0,
                },
                |k| {
                    let (name_kind, pos, expr) = (
                        defs[3 * k].to_native(),
                        defs[3 * k + 1].to_native(),
                        defs[3 * k + 2].to_native(),
                    );
                    Ok(AttrDef {
                        name: check.sym(name_kind & (SYM_LIMIT - 1))?,
                        pos: check.pos(pos)?,
                        kind: match name_kind >> 30 {
                            0 => AttrKind::Plain,
                            1 => AttrKind::Inherited,
                            2 => AttrKind::InheritedFrom,
                            _ => return Err(bad("bad attribute kind")),
                        },
                        expr: check.node(expr)?,
                        displ: if explicit {
                            displs[k].to_native()
                        } else {
                            k as u32
                        },
                    })
                },
            )?;
            let dynamic_attrs: &'a [DynamicAttrDef] = try_fill(
                bump,
                n_dyn,
                || DynamicAttrDef {
                    name: NodeId(0),
                    value: NodeId(0),
                    pos: Pos::NONE,
                },
                |k| {
                    Ok(DynamicAttrDef {
                        name: check.node(dynamic[3 * k].to_native())?,
                        value: check.node(dynamic[3 * k + 1].to_native())?,
                        pos: check.pos(dynamic[3 * k + 2].to_native())?,
                    })
                },
            )?;
            let inherit_from: &'a [NodeId] = try_fill(
                bump,
                n_from,
                || NodeId(0),
                |k| check.node(from[k].to_native()),
            )?;
            Ok(AttrSet {
                recursive: flags & 1 != 0,
                pos: check.pos(pos)?,
                attrs,
                id_order: id_order(bump, n, |i| attrs[i].name)?,
                dynamic_attrs,
                inherit_from,
            })
        })?;
        if d * 3 != defs_w.len() || x != displs_w.len() || y * 3 != dyn_w.len() || z != from_w.len()
        {
            return Err(bad("attribute tables not used up"));
        }
        Ok(sets)
    }

    fn link_lambdas<'a>(&self, bump: &'a Bump, check: &Check<'_>) -> Result<&'a [Lambda<'a>], Bad> {
        let sec = &self.sections;
        let formals_w = sec[s::FORMALS];
        let mut records = sec[s::LAMBDAS];
        let mut f = 0usize;
        let lambdas = try_fill(
            bump,
            check.lambdas as usize,
            || Lambda {
                pos: Pos::NONE,
                name: None,
                arg: None,
                formals: None,
                body: NodeId(0),
            },
            |_| {
                let [pos, name, arg, body, flag, n] = take(&mut records)?;
                let n = n as usize;
                let formals = match flag {
                    0 if n == 0 => None,
                    1 | 2 => {
                        let words = span(formals_w, f, n, 3)?;
                        f += n;
                        let formals: &'a [Formal] = try_fill(
                            bump,
                            n,
                            || Formal {
                                pos: Pos::NONE,
                                name: Symbol(0),
                                default: None,
                            },
                            |k| {
                                Ok(Formal {
                                    pos: check.pos(words[3 * k].to_native())?,
                                    name: check.sym(words[3 * k + 1].to_native())?,
                                    default: check.opt_node(words[3 * k + 2].to_native())?,
                                })
                            },
                        )?;
                        Some(Formals {
                            formals,
                            id_order: id_order(bump, n, |i| formals[i].name)?,
                            ellipsis: flag == 2,
                        })
                    }
                    _ => return Err(bad("bad formals flag")),
                };
                Ok(Lambda {
                    pos: check.pos(pos)?,
                    name: check.opt_sym(name)?,
                    arg: check.opt_sym(arg)?,
                    formals,
                    body: check.node(body)?,
                })
            },
        )?;
        if f * 3 != formals_w.len() {
            return Err(bad("formal tables not used up"));
        }
        Ok(lambdas)
    }
}

// ---------------------------------------------------------------------
// Line tables
// ---------------------------------------------------------------------

/// A module's line table (`brix_syntax::source::line_starts`) as LEB128
/// deltas between successive line starts. Kept compact: it is decoded
/// only when a position in the module is shown.
pub(crate) fn pack_line_starts(starts: &[u32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(starts.len() * 2);
    let mut put = |mut v: u32| {
        while v >= 0x80 {
            buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    };
    put(starts.len() as u32);
    let mut prev = 0u32;
    for &s in starts {
        put(s.wrapping_sub(prev));
        prev = s;
    }
    buf
}

/// Reverses [`pack_line_starts`], checking that the result is a line
/// table of a file of `len` bytes: it starts at 0 and never decreases
/// or passes the end.
pub(crate) fn unpack_line_starts(packed: &[u8], len: u32) -> Result<Vec<u32>, ImageError> {
    let corrupt = || ImageError::Corrupt("damaged line table".into());
    let mut at = 0usize;
    let mut get = || -> Result<u32, ImageError> {
        let mut v = 0u64;
        for shift in (0..35).step_by(7) {
            let b = *packed.get(at).ok_or_else(corrupt)?;
            at += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return u32::try_from(v).map_err(|_| corrupt());
            }
        }
        Err(corrupt())
    };
    let n = get()? as usize;
    // Every entry takes at least one byte.
    if n == 0 || n > packed.len() {
        return Err(corrupt());
    }
    let mut starts = Vec::with_capacity(n);
    let mut prev = 0u32;
    for _ in 0..n {
        prev = prev.checked_add(get()?).ok_or_else(corrupt)?;
        starts.push(prev);
    }
    if at != packed.len() || starts[0] != 0 || prev > len {
        return Err(corrupt());
    }
    Ok(starts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{AttrSet as IrAttrSet, Formals as IrFormals, Lambda as IrLambda};
    use crate::symbol::{SymbolKeys, SymbolTable};

    const BASE: u32 = 4;

    fn sample() -> Code {
        let sym = |n: u32| Symbol(BASE + n);
        Code {
            root: NodeId(4),
            nodes: vec![
                Node::Int(-5),
                Node::Float(2.5),
                Node::Str(LitId(0)),
                Node::Path {
                    anchor: PathAnchor::Source,
                    lit: LitId(1),
                },
                Node::Call {
                    pos: Pos(3),
                    fun: NodeId(5),
                    args: Range { start: 0, len: 2 },
                },
                Node::Var {
                    pos: Pos(0),
                    name: Symbol(3),
                    var: VarRef::Env { level: 1, displ: 2 },
                },
                Node::With {
                    pos: Pos(9),
                    attrs: NodeId(8),
                    body: NodeId(7),
                    prev_with: 0,
                    parent_with: None,
                },
                Node::Var {
                    pos: Pos::NONE,
                    name: sym(1),
                    var: VarRef::With {
                        level: 0,
                        with: NodeId(6),
                    },
                },
                Node::Attrs(AttrsId(0)),
                Node::Select {
                    pos: Pos(11),
                    subject: NodeId(8),
                    path: Range { start: 0, len: 2 },
                    default: Some(NodeId(0)),
                },
                Node::HasAttr {
                    subject: NodeId(8),
                    path: Range { start: 1, len: 1 },
                },
                Node::List {
                    elems: Range { start: 0, len: 2 },
                },
                Node::Lambda(LambdaId(0)),
                Node::Let {
                    attrs: AttrsId(1),
                    body: NodeId(0),
                },
                Node::If {
                    pos: Pos(1),
                    cond: NodeId(0),
                    then_: NodeId(1),
                    else_: NodeId(2),
                },
                Node::Assert {
                    pos: Pos(2),
                    cond: NodeId(0),
                    body: NodeId(1),
                },
                Node::Not(NodeId(0)),
                Node::BinOp {
                    pos: Pos(4),
                    op: BinOp::Update,
                    lhs: NodeId(8),
                    rhs: NodeId(8),
                },
                Node::ConcatStrings {
                    pos: Pos(5),
                    force_string: true,
                    parts: Range { start: 0, len: 2 },
                },
                Node::CurPos(Pos(6)),
                Node::InheritFrom {
                    pos: Pos(7),
                    displ: 0,
                },
                Node::Path {
                    anchor: PathAnchor::Source,
                    lit: LitId(2),
                },
                Node::Path {
                    anchor: PathAnchor::Source,
                    lit: LitId(3),
                },
                Node::Path {
                    anchor: PathAnchor::Root,
                    lit: LitId(4),
                },
                Node::Path {
                    anchor: PathAnchor::Source,
                    lit: LitId(5),
                },
                Node::Select {
                    pos: Pos(12),
                    subject: NodeId(8),
                    path: Range { start: 0, len: 1 },
                    default: None,
                },
                Node::With {
                    pos: Pos(13),
                    attrs: NodeId(8),
                    body: NodeId(0),
                    prev_with: 2,
                    parent_with: Some(NodeId(6)),
                },
            ],
            strings: vec![
                b"hello".to_vec(),
                b"/__brix_image_root__/pkgs/a/b/foo.nix".to_vec(),
                b"/__brix_image_root__/pkgs/x".to_vec(),
                b"/__brix_image_root__".to_vec(),
                b"/etc/passwd".to_vec(),
                b"/__brix_image_root__/pkgs/a/b/dir/".to_vec(),
            ],
            attr_sets: vec![
                IrAttrSet {
                    recursive: true,
                    pos: Pos(8),
                    attrs: vec![
                        AttrDef {
                            name: Symbol(3),
                            pos: Pos(8),
                            kind: AttrKind::Plain,
                            expr: NodeId(0),
                            displ: 0,
                        },
                        AttrDef {
                            name: sym(0),
                            pos: Pos::NONE,
                            kind: AttrKind::InheritedFrom,
                            expr: NodeId(9),
                            displ: 1,
                        },
                        AttrDef {
                            name: sym(2),
                            pos: Pos(10),
                            kind: AttrKind::Inherited,
                            expr: NodeId(5),
                            displ: 2,
                        },
                    ],
                    dynamic_attrs: vec![DynamicAttrDef {
                        name: NodeId(2),
                        value: NodeId(0),
                        pos: Pos(10),
                    }],
                    inherit_from: vec![NodeId(8)],
                },
                // Displacements that are not the index.
                IrAttrSet {
                    recursive: false,
                    pos: Pos::NONE,
                    attrs: vec![AttrDef {
                        name: Symbol(1),
                        pos: Pos(1),
                        kind: AttrKind::Inherited,
                        expr: NodeId(5),
                        displ: 7,
                    }],
                    dynamic_attrs: vec![],
                    inherit_from: vec![],
                },
            ],
            lambdas: vec![
                IrLambda {
                    pos: Pos(12),
                    name: Some(Symbol(3)),
                    arg: None,
                    formals: Some(IrFormals {
                        formals: vec![
                            Formal {
                                pos: Pos(13),
                                name: sym(1),
                                default: None,
                            },
                            Formal {
                                pos: Pos(14),
                                name: sym(0),
                                default: Some(NodeId(0)),
                            },
                        ],
                        ellipsis: true,
                    }),
                    body: NodeId(0),
                },
                IrLambda {
                    pos: Pos(15),
                    name: None,
                    arg: Some(sym(2)),
                    formals: None,
                    body: NodeId(1),
                },
            ],
            attr_names: vec![AttrName::Static(Symbol(3)), AttrName::Dynamic(NodeId(2))],
            node_lists: vec![NodeId(0), NodeId(1)],
            parts: vec![(Pos(0), NodeId(2)), (Pos::NONE, NodeId(1))],
        }
    }

    fn words(e: &Encoded) -> Vec<Word> {
        e.words.iter().map(|&w| Word::from_native(w)).collect()
    }

    /// Links `encoded` against a symbol table where the module's new
    /// names get `syms` (interned by the caller), returning the tables
    /// in `ir::Code` form for comparison.
    fn link_back(encoded: &Encoded, mount: &[u8]) -> Result<Code, Bad> {
        let bump = Bump::new();
        let keys = SymbolKeys::new(&bump);
        let mut table = SymbolTable::new(keys);
        let ws = words(encoded);
        let m = ModuleCode::parse(&ws, &encoded.strings)?;
        // New names interned in list order; the list is n0, n1, n2.
        let syms: Vec<Symbol> = m
            .new_names()
            .map(|i| table.intern(format!("n{i}").as_bytes()))
            .collect();
        let linked = m.link(&bump, BASE, &syms, 1000, mount)?;
        // Map evaluator symbols back to the compiling ids for comparison.
        let back = |s: Symbol| -> Symbol {
            match syms.iter().position(|&x| x == s) {
                Some(k) => Symbol(BASE + k as u32),
                None => s,
            }
        };
        let code = Code {
            root: linked.root,
            nodes: linked
                .nodes
                .iter()
                .map(|n| match *n {
                    Node::Var { pos, name, var } => Node::Var {
                        pos,
                        name: back(name),
                        var,
                    },
                    other => other,
                })
                .collect(),
            strings: linked.strings.iter().map(|s| s.to_vec()).collect(),
            attr_sets: linked
                .attr_sets
                .iter()
                .map(|s| {
                    // The id order must be the permutation the names say.
                    let ids: Vec<Symbol> = s.in_id_order().map(|(_, a)| a.name).collect();
                    assert!(ids.windows(2).all(|w| w[0] < w[1]));
                    IrAttrSet {
                        recursive: s.recursive,
                        pos: s.pos,
                        attrs: s
                            .attrs
                            .iter()
                            .map(|a| AttrDef {
                                name: back(a.name),
                                ..*a
                            })
                            .collect(),
                        dynamic_attrs: s.dynamic_attrs.to_vec(),
                        inherit_from: s.inherit_from.to_vec(),
                    }
                })
                .collect(),
            lambdas: linked
                .lambdas
                .iter()
                .map(|l| IrLambda {
                    pos: l.pos,
                    name: l.name.map(back),
                    arg: l.arg.map(back),
                    formals: l.formals.as_ref().map(|f| IrFormals {
                        formals: f
                            .formals
                            .iter()
                            .map(|x| Formal {
                                name: back(x.name),
                                ..*x
                            })
                            .collect(),
                        ellipsis: f.ellipsis,
                    }),
                    body: l.body,
                })
                .collect(),
            attr_names: linked
                .attr_names
                .iter()
                .map(|n| match *n {
                    AttrName::Static(s) => AttrName::Static(back(s)),
                    d => d,
                })
                .collect(),
            node_lists: linked.node_lists.to_vec(),
            parts: linked.parts.to_vec(),
        };
        Ok(code)
    }

    fn encoded() -> Encoded {
        encode(&sample(), &[0, 1, 2]).expect("encodes")
    }

    #[test]
    fn round_trips_every_node_kind() {
        let code = sample();
        let mut expected = code.clone();
        // Source-relative paths come back under the mount point.
        for s in &mut expected.strings {
            if let Some(rest) = s.strip_prefix(ROOT_MARKER.as_bytes()) {
                *s = [b"/mnt/x".as_slice(), rest].concat();
            }
        }
        let back = link_back(&encoded(), b"/mnt/x").expect("links");
        assert_eq!(format!("{expected:?}"), format!("{back:?}"));
        // Mounted at `/`, the root itself is `/`.
        let at_root = link_back(&encoded(), b"").expect("links");
        assert_eq!(at_root.strings[3], b"/");
        assert_eq!(at_root.strings[1], b"/pkgs/a/b/foo.nix");
    }

    /// Path literals are bytes, as in Nix: a name that is not UTF-8 (a
    /// Latin-1 directory) comes back exactly as written, under a mount
    /// point that is not UTF-8 either, and an absolute literal is kept
    /// as it is.
    #[test]
    fn path_literals_keep_their_bytes() {
        let mut code = sample();
        code.strings[2] = b"/__brix_image_root__/d\xe9/caf\xe9.nix".to_vec();
        code.strings[4] = b"/r\xff".to_vec();
        let e = encode(&code, &[0, 1, 2]).expect("encodes");
        let back = link_back(&e, b"/mnt/\xff").expect("links");
        assert_eq!(back.strings[2], b"/mnt/\xff/d\xe9/caf\xe9.nix");
        assert_eq!(back.strings[3], b"/mnt/\xff");
        assert_eq!(back.strings[4], b"/r\xff");
    }

    #[test]
    fn rejects_damage() {
        let e = encoded();
        // Every truncation is rejected rather than read out of bounds.
        for cut in 0..e.words.len() {
            let damaged = Encoded {
                words: e.words[..cut].to_vec(),
                strings: e.strings.clone(),
            };
            assert!(link_back(&damaged, b"/m").is_err(), "cut at {cut}");
        }
        // Every single-word change either fails cleanly or links (a
        // changed position or symbol can still be in range); none panics.
        for i in 0..e.words.len() {
            for v in [0, 1, 2, 0xff, 0x100, 0x7fff_ffff, 0xffff_fffe, u32::MAX] {
                let mut words = e.words.clone();
                words[i] = v;
                let _ = link_back(
                    &Encoded {
                        words,
                        strings: e.strings.clone(),
                    },
                    b"/m",
                );
                let mut words = e.words.clone();
                words[i] ^= 1 << (v % 32);
                let _ = link_back(
                    &Encoded {
                        words,
                        strings: e.strings.clone(),
                    },
                    b"/m",
                );
            }
        }
        // Specific damage is caught.
        let corrupt = |f: &dyn Fn(&mut Vec<u32>)| {
            let mut words = e.words.clone();
            f(&mut words);
            link_back(
                &Encoded {
                    words,
                    strings: e.strings.clone(),
                },
                b"/m",
            )
            .err()
            .map(|b| b.0)
        };
        let nodes_at = HEADER_WORDS + 3 + 6;
        // The first node's tag.
        assert_eq!(corrupt(&|w| w[nodes_at] = 0xee), Some("unknown node tag"));
        // A header count that would make the loader allocate wildly.
        assert!(corrupt(&|w| w[h::NODES] = u32::MAX).is_some());
        // The string ends running past the bytes.
        assert_eq!(
            corrupt(&|w| w[HEADER_WORDS + 3] = 1 << 20),
            Some("string out of range")
        );
    }

    #[test]
    fn with_variables_must_name_an_enclosing_with() {
        let mut code = sample();
        // Point the `with` variable at a non-`with` node.
        code.nodes[7] = Node::Var {
            pos: Pos::NONE,
            name: Symbol(BASE + 1),
            var: VarRef::With {
                level: 0,
                with: NodeId(5),
            },
        };
        let e = encode(&code, &[0, 1, 2]).unwrap();
        assert_eq!(
            link_back(&e, b"/m").err().map(|b| b.0),
            Some("with variable not under a with")
        );
        // A `with` that is its own parent would make lookups loop.
        let mut code = sample();
        code.nodes[26] = Node::With {
            pos: Pos(13),
            attrs: NodeId(8),
            body: NodeId(0),
            prev_with: 2,
            parent_with: Some(NodeId(26)),
        };
        let e = encode(&code, &[0, 1, 2]).unwrap();
        assert_eq!(
            link_back(&e, b"/m").err().map(|b| b.0),
            Some("with parent is not an enclosing with")
        );
    }

    #[test]
    fn repeated_names_are_damage() {
        // Two attributes of one set with the same name.
        let mut code = sample();
        code.attr_sets[0].attrs[2].name = code.attr_sets[0].attrs[1].name;
        let e = encode(&code, &[0, 1, 2]).unwrap();
        assert_eq!(
            link_back(&e, b"/m").err().map(|b| b.0),
            Some("repeated name")
        );
        // Two formals of one lambda with the same name.
        let mut code = sample();
        let formals = &mut code.lambdas[0].formals.as_mut().unwrap().formals;
        formals[1].name = formals[0].name;
        let e = encode(&code, &[0, 1, 2]).unwrap();
        assert_eq!(
            link_back(&e, b"/m").err().map(|b| b.0),
            Some("repeated name")
        );
    }

    #[test]
    fn unencodable_modules_are_refused() {
        let mut code = sample();
        code.nodes[5] = Node::Var {
            pos: Pos(0),
            name: Symbol(3),
            var: VarRef::Env {
                level: 1 << 24,
                displ: 2,
            },
        };
        assert!(encode(&code, &[0, 1, 2]).is_err());
        let mut code = sample();
        code.strings[1] = b"/elsewhere/foo.nix".to_vec();
        assert!(encode(&code, &[0, 1, 2]).is_err());
    }

    #[test]
    fn records_are_compact() {
        // Variables, selections and calls take four words; literals two
        // or three.
        let e = encoded();
        let sizes = section_sizes(&e.words).unwrap();
        let nodes = sizes.iter().find(|(n, _)| *n == "nodes").unwrap().1;
        assert!(nodes <= sample().nodes.len() * 4, "{nodes} words");
    }

    #[test]
    fn line_tables_round_trip() {
        let starts = vec![0u32, 5, 6, 30, 31, 1000];
        assert_eq!(
            unpack_line_starts(&pack_line_starts(&starts), 1000).unwrap(),
            starts
        );
        assert_eq!(
            unpack_line_starts(&pack_line_starts(&[0]), 0).unwrap(),
            vec![0]
        );
        // Past the end of the file, not starting at 0, trailing bytes,
        // truncated, empty.
        assert!(unpack_line_starts(&pack_line_starts(&starts), 999).is_err());
        assert!(unpack_line_starts(&pack_line_starts(&[1, 2]), 10).is_err());
        assert!(unpack_line_starts(&[5, 1], 10).is_err());
        assert!(unpack_line_starts(&[2, 0], 10).is_err());
        assert!(unpack_line_starts(&[0], 10).is_err());
        assert!(unpack_line_starts(&[], 10).is_err());
    }
}
