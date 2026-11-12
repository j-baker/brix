//! Attribute sets. (`Bindings` in Nix.)
//!
//! Nix keeps a set's attributes in an array sorted by symbol id and copies
//! the whole array on every `//`. nixpkgs merges sets of twenty thousand
//! attributes many times over (its overlay stack is `prev // f final prev`
//! per layer, `callPackage` intersects with the whole package set, and
//! `mkDerivation` merges a handful of sets per package), so for
//! `firefox.drvPath` that is ten million attribute copies.
//!
//! brix uses two representations behind one API:
//!
//! * **Flat**: a sorted array of 16-byte [`Attr`]s, for sets of up to
//!   [`FLAT_MAX`] attributes. Lookups are a linear scan for tiny sets and a
//!   binary search otherwise; merges copy, which for small sets is the
//!   cheapest thing there is.
//! * **Tree**: a persistent B+-tree with wide, immutable, arena-allocated
//!   nodes for larger sets. Inserting a few
//!   attributes into a set of twenty thousand copies the path to the
//!   touched leaves (a few hundred bytes) instead of the whole set, and
//!   every untouched subtree is shared with the original. Lookups are a few
//!   node hops; in-order iteration is a tree walk.
//!
//! A [`Bindings`] is a pointer-sized, copyable handle to the set's header
//! (the private `raw` module has the layout), which holds the set's
//! position and either the attribute slice or the tree's root, so a
//! [`crate::value::Value`] holding a set stays 16 bytes.
//!
//! The canonical order is Nix's: **by symbol id**, that is by the order in
//! which the names were first interned ([`Symbol`]'s `Ord`). Lookups,
//! `//`, `removeAttrs`, `intersectAttrs` and formal-argument matching
//! therefore compare two `u32`s and never look at the names. Because brix
//! creates symbols in the order Nix does (see [`crate::symbol`]),
//! iterating a set ([`Bindings::iter`]) visits the attributes in the order
//! Nix's `for (auto & a : *bindings)` does, which is observable wherever
//! attributes are forced in set order: `==`, `deepSeq`, `--strict`, and
//! the argument loops of primops such as `builtins.path` or `fetchTree`.
//!
//! Name order (the lexicographic byte order of the names) is what
//! `attrNames`, `attrValues`, the printers, `toJSON`, `toXML` and
//! `derivationStrict` produce. Nix sorts for each of those
//! (`Bindings::lexicographicOrder`, or a `std::sort` by name in the
//! primop), and so does brix, at the same places: [`Bindings::lexicographic`]
//! sorts a set's attributes by name through the evaluator's
//! [`SymbolKeys`].
//!
//! Invariants: attributes are unique by name and in canonical order; a tree
//! has at least `FLAT_MAX + 1` attributes, every leaf is non-empty, and an
//! inner node's `firsts[i]` is the first name in `children[i]`. Leaves and
//! inner nodes may be under-full after removals (there is no rebalancing),
//! which bounds nothing but is harmless: the height never grows through a
//! removal. A set's identity (the arena address of its header,
//! [`Bindings::addr`]) is observable through the printers' `«repeated»`
//! markers, so sets are never copied when they do not need to be.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::runtime::Bump;
use smallvec::SmallVec;

use crate::symbol::{Symbol, SymbolKeys};
use crate::value::{SrcPos, ValueRef};

pub use raw::Bindings;
use raw::Repr;

/// An attribute of a set. (`Attr` in Nix.) 16 bytes.
///
/// Two measured alternatives were rejected while sets were still kept in
/// name order, when every comparison went through `SymbolKeys`: storing
/// the name's eight-byte prefix inline, as a 24-byte entry, cut the
/// comparison share of the profile from 6.1% to 4.6% but raised merge
/// copying from 2.5% to 3.5%, for no change in wall or CPU time; and a SIMD
/// "count the keys below the probe" search lost to binary search at every
/// size in play (n = 16: 1.8 ns vs 2.2–2.4 ns for AVX2/AVX-512; n = 64:
/// 2.8 ns vs 6.1–7.2 ns). Ordering by symbol id made the comparisons plain
/// integer ones instead.
#[derive(Clone, Copy, Debug)]
pub struct Attr<'a> {
    pub name: Symbol,
    pub pos: SrcPos,
    pub value: ValueRef<'a>,
}

impl<'a> Attr<'a> {
    pub fn new(name: Symbol, pos: SrcPos, value: ValueRef<'a>) -> Attr<'a> {
        Attr { name, pos, value }
    }
}

// An attribute is a symbol, a position and a pointer, without padding; a
// set's handle is one pointer, so `Value::Attrs` stays 16 bytes.
const _: () = assert!(std::mem::size_of::<Attr<'static>>() == 8 + std::mem::size_of::<usize>());
const _: () = assert!(std::mem::size_of::<Bindings<'static>>() == std::mem::size_of::<usize>());
const _: () =
    assert!(std::mem::size_of::<Option<Bindings<'static>>>() == std::mem::size_of::<usize>());

/// Sets with at most this many attributes are flat slices. Chosen so that
/// `mkDerivation`-sized sets (30 to 60 attributes) stay flat: a tree with a
/// single leaf would cost the same copy plus a node.
pub const FLAT_MAX: usize = 64;
/// Attributes per leaf, at most.
const LEAF_MAX: usize = 64;
/// Children per inner node, at most.
const INNER_MAX: usize = 64;
/// Flat sets up to this size are searched linearly rather than by
/// bisection.
const LINEAR_MAX: usize = 8;

/// The arena layout of a set.
///
/// A set is a `Header` in the arena: its position and what it holds.
///
/// ```text
/// flat (len <= FLAT_MAX):  pos | Flat(&[Attr])           the attributes, in a second allocation
/// tree (len >  FLAT_MAX):  pos | Tree { root: &Node, len }
/// ```
///
/// A [`Bindings`] is a reference to the header, one word wide (a slice
/// reference held directly would be two words and make
/// [`crate::value::Value`] 24 bytes). The header is immutable and has no
/// interior mutability: attribute values are cells elsewhere in the arena.
/// [`Bindings::new_flat`] and [`Bindings::new_tree`] are the only ways to
/// make a set, and keep the two shapes on their sides of [`FLAT_MAX`].
mod raw {
    use crate::runtime::Bump;

    use super::{Attr, Node, FLAT_MAX};
    use crate::value::SrcPos;

    struct Header<'a> {
        /// Position of the set's opening brace (`Bindings::pos`).
        pos: SrcPos,
        repr: Repr<'a>,
    }

    /// An attribute set: a handle to its arena header. Copying it copies the
    /// pointer; two handles are the same set when [`Bindings::addr`] agrees.
    #[derive(Clone, Copy)]
    pub struct Bindings<'a>(&'a Header<'a>);

    /// What a set holds, read through [`Bindings::repr`].
    #[derive(Clone, Copy)]
    pub(super) enum Repr<'a> {
        Flat(&'a [Attr<'a>]),
        Tree { root: &'a Node<'a>, len: u32 },
    }

    impl<'a> Bindings<'a> {
        /// A flat set of `len` attributes (at most [`FLAT_MAX`]), the `k`-th
        /// being `attr(k)`.
        pub(super) fn new_flat(
            bump: &'a Bump,
            pos: SrcPos,
            len: usize,
            attr: impl FnMut(usize) -> Attr<'a>,
        ) -> Bindings<'a> {
            assert!(
                len <= FLAT_MAX,
                "a flat set holds at most FLAT_MAX attributes"
            );
            let attrs: &'a [Attr<'a>] = bump.alloc_slice_fill_with(len, attr);
            Bindings(bump.alloc(Header {
                pos,
                repr: Repr::Flat(attrs),
            }))
        }

        /// A tree-shaped set of `len` attributes (more than [`FLAT_MAX`])
        /// under `root`.
        pub(super) fn new_tree(
            bump: &'a Bump,
            pos: SrcPos,
            len: usize,
            root: &'a Node<'a>,
        ) -> Bindings<'a> {
            assert!(len > FLAT_MAX, "a tree holds more than FLAT_MAX attributes");
            let len = u32::try_from(len).expect("more than 4 billion attributes in a set");
            Bindings(bump.alloc(Header {
                pos,
                repr: Repr::Tree { root, len },
            }))
        }

        /// The position of the set's opening brace, for `unsafeGetAttrPos`-
        /// style error locations.
        pub fn pos(self) -> SrcPos {
            self.0.pos
        }

        pub fn len(self) -> usize {
            match self.0.repr {
                Repr::Flat(attrs) => attrs.len(),
                Repr::Tree { len, .. } => len as usize,
            }
        }

        pub(super) fn repr(self) -> Repr<'a> {
            self.0.repr
        }

        /// The set's identity: the address of its header. Nix's printers
        /// mark a set they have already shown by this (`«repeated»`).
        pub fn addr(self) -> usize {
            self.0 as *const Header<'a> as usize
        }
    }
}

impl std::fmt::Debug for Bindings<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bindings")
            .field("pos", &self.pos())
            .field("len", &self.len())
            .field("tree", &self.is_tree())
            .finish()
    }
}

enum Node<'a> {
    Leaf(&'a [Attr<'a>]),
    Inner {
        children: &'a [&'a Node<'a>],
        /// `firsts[i]` is the first attribute name in `children[i]`.
        firsts: &'a [Symbol],
    },
}

impl<'a> Node<'a> {
    fn first(&self) -> Symbol {
        match self {
            Node::Leaf(attrs) => attrs[0].name,
            Node::Inner { firsts, .. } => firsts[0],
        }
    }
}

/// Searches a canonically sorted slice.
#[inline]
fn flat_get<'a>(attrs: &'a [Attr<'a>], name: Symbol) -> Option<&'a Attr<'a>> {
    if attrs.len() <= LINEAR_MAX {
        return attrs.iter().find(|a| a.name == name);
    }
    attrs
        .binary_search_by_key(&name, |a| a.name)
        .ok()
        .map(|i| &attrs[i])
}

/// The child of an inner node that may contain `name`: the last one whose
/// first name is not greater, or the first child for names below them all.
fn child_index(firsts: &[Symbol], name: Symbol) -> usize {
    firsts.partition_point(|&f| f <= name).max(1) - 1
}

impl<'a> Bindings<'a> {
    /// The empty set.
    pub fn empty(bump: &'a Bump, pos: SrcPos) -> Bindings<'a> {
        Bindings::new_flat(bump, pos, 0, |_| {
            unreachable!("the empty set has no attributes")
        })
    }

    /// A set of `len` attributes in canonical order (unique names in
    /// ascending symbol id), the `k`-th being `attr(k)`. Each attribute is
    /// written once: into the set's own slice when it is flat, into the
    /// slice its tree's leaves share otherwise.
    pub fn from_fn(
        bump: &'a Bump,
        len: usize,
        pos: SrcPos,
        attr: impl FnMut(usize) -> Attr<'a>,
    ) -> Bindings<'a> {
        if len <= FLAT_MAX {
            return Bindings::new_flat(bump, pos, len, attr);
        }
        let attrs: &'a [Attr<'a>] = bump.alloc_slice_fill_with(len, attr);
        let leaves: Vec<&'a Node<'a>> = even_chunks(attrs.len(), LEAF_MAX)
            .map(|range| &*bump.alloc(Node::Leaf(&attrs[range])))
            .collect();
        Bindings::new_tree(bump, pos, len, build_above(bump, leaves))
    }

    /// A set over attributes already in canonical order.
    pub fn from_sorted(bump: &'a Bump, attrs: &[Attr<'a>], pos: SrcPos) -> Bindings<'a> {
        Bindings::from_fn(bump, attrs.len(), pos, |k| attrs[k])
    }

    /// Sorts attributes into canonical order, later duplicates winning.
    pub fn sort_dedup(attrs: &mut Vec<Attr<'a>>) {
        // Stable, so among duplicates the later one is last and survives.
        attrs.sort_by_key(|a| a.name);
        attrs.dedup_by(|later, earlier| {
            if later.name == earlier.name {
                *earlier = *later;
                true
            } else {
                false
            }
        });
    }

    pub fn get(self, name: Symbol) -> Option<&'a Attr<'a>> {
        match self.repr() {
            Repr::Flat(attrs) => flat_get(attrs, name),
            Repr::Tree { root, .. } => {
                let mut node = root;
                loop {
                    match node {
                        Node::Leaf(attrs) => return flat_get(attrs, name),
                        Node::Inner { children, firsts } => {
                            node = children[child_index(firsts, name)];
                        }
                    }
                }
            }
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Whether the set is stored as a tree (for tests and statistics).
    pub fn is_tree(self) -> bool {
        self.len() > FLAT_MAX
    }

    /// Whether the attributes are unique and in canonical order (for
    /// assertions).
    pub fn is_canonical(self) -> bool {
        let mut it = self.iter();
        let Some(mut prev) = it.next() else {
            return true;
        };
        it.all(|a| {
            let ordered = prev.name < a.name;
            prev = a;
            ordered
        })
    }

    /// The attributes in canonical order: by symbol id, the order in which
    /// Nix iterates a `Bindings`.
    pub fn iter(self) -> Iter<'a> {
        match self.repr() {
            Repr::Flat(attrs) => Iter {
                stack: SmallVec::new(),
                leaf: attrs.iter(),
                remaining: attrs.len(),
            },
            Repr::Tree { root, len } => Iter::over(root, len as usize),
        }
    }

    /// The attributes in lexicographic order of their names: Nix's
    /// `Bindings::lexicographicOrder`, and the order of `attrNames`,
    /// `attrValues` and the printers. A sort, as in Nix; each name's
    /// comparison prefix is read once rather than once per comparison.
    pub fn lexicographic(self, keys: &SymbolKeys<'a>) -> Vec<&'a Attr<'a>> {
        let mut keyed: Vec<(u64, &'a Attr<'a>)> =
            self.iter().map(|a| (keys.prefix(a.name), a)).collect();
        // Names are unique, so an unstable sort gives the one order there is.
        keyed.sort_unstable_by(|(px, x), (py, y)| match px.cmp(py) {
            Ordering::Equal => keys.name(x.name).cmp(keys.name(y.name)),
            o => o,
        });
        keyed.into_iter().map(|(_, a)| a).collect()
    }

    /// The attributes as one slice, when the set is flat.
    #[cfg(test)]
    pub fn as_flat(self) -> Option<&'a [Attr<'a>]> {
        match self.repr() {
            Repr::Flat(attrs) => Some(attrs),
            Repr::Tree { .. } => None,
        }
    }

    /// [`Bindings::update_counted`] without the count.
    #[cfg(test)]
    pub fn update(bump: &'a Bump, left: Bindings<'a>, right: Bindings<'a>) -> Bindings<'a> {
        Bindings::update_counted(bump, left, right).0
    }

    /// `left // right`: the attributes of both, `right` winning on a shared
    /// name, and how many attributes were physically written (the cost
    /// the `//` histogram records). The result has no position:
    /// `ExprOpUpdate::eval` builds a fresh `Bindings`, whose `pos` is
    /// `noPos`. Neither operand may be empty (callers return the other
    /// operand itself then, which keeps its identity).
    pub fn update_counted(
        bump: &'a Bump,
        left: Bindings<'a>,
        right: Bindings<'a>,
    ) -> (Bindings<'a>, usize) {
        debug_assert!(!left.is_empty() && !right.is_empty());
        let pos = SrcPos::NONE;
        match (left.repr(), right.repr()) {
            (Repr::Tree { root, len }, _) if path_copy_pays(len as usize, right.len()) => {
                let merged = insert_all(bump, root, &right.contiguous(), true);
                let written = merged.written;
                (
                    Bindings::from_merged(bump, merged, len as usize, pos),
                    written,
                )
            }
            (_, Repr::Tree { root, len }) if path_copy_pays(len as usize, left.len()) => {
                let merged = insert_all(bump, root, &left.contiguous(), false);
                let written = merged.written;
                (
                    Bindings::from_merged(bump, merged, len as usize, pos),
                    written,
                )
            }
            // Two flat sets (the commonest case), or a tree too small for
            // path copying to pay: merge over the attributes as slices,
            // copying a tree's leaves into one first.
            _ => {
                let merged = merge_slices(bump, pos, &left.contiguous(), &right.contiguous());
                (merged, merged.len())
            }
        }
    }

    /// The set without the given names (`removeAttrs`). `names` need not be
    /// sorted or unique. Always a fresh set without a position, as
    /// `prim_removeAttrs` builds it.
    pub fn remove(bump: &'a Bump, set: Bindings<'a>, names: &[Symbol]) -> Bindings<'a> {
        let pos = SrcPos::NONE;
        let mut names: Vec<Symbol> = names.to_vec();
        names.sort_unstable();
        names.dedup();
        match set.repr() {
            Repr::Flat(attrs) => {
                let kept = filter_out(attrs, &names);
                Bindings::from_sorted(bump, &kept, pos)
            }
            Repr::Tree { root, len } => {
                let (node, removed) = remove_all(bump, root, &names);
                let new_len = len as usize - removed;
                match node {
                    None => Bindings::empty(bump, pos),
                    Some(root) if new_len <= FLAT_MAX => {
                        let mut it = Iter::over(root, new_len);
                        Bindings::from_fn(bump, new_len, pos, |_| {
                            *it.next().expect("the tree holds new_len attributes")
                        })
                    }
                    Some(root) => Bindings::new_tree(bump, pos, new_len, root),
                }
            }
        }
    }

    /// A set with the same names and `f` applied to each attribute, in
    /// canonical order (`mapAttrs` and friends).
    pub fn map(
        bump: &'a Bump,
        set: Bindings<'a>,
        pos: SrcPos,
        mut f: impl FnMut(&'a Attr<'a>) -> Attr<'a>,
    ) -> Bindings<'a> {
        let mut it = set.iter();
        Bindings::from_fn(bump, set.len(), pos, |_| {
            let a = it
                .next()
                .expect("iterating as many attributes as the set has");
            let n = f(a);
            debug_assert_eq!(n.name, a.name);
            n
        })
    }

    /// The attributes as one slice: a flat set's own, or a copy of a
    /// tree's leaves.
    fn contiguous(self) -> Cow<'a, [Attr<'a>]> {
        match self.repr() {
            Repr::Flat(attrs) => Cow::Borrowed(attrs),
            Repr::Tree { root, len } => {
                let mut attrs = Vec::with_capacity(len as usize);
                collect_leaves(root, &mut attrs);
                Cow::Owned(attrs)
            }
        }
    }

    fn from_merged(
        bump: &'a Bump,
        merged: Merged<'a>,
        old_len: usize,
        pos: SrcPos,
    ) -> Bindings<'a> {
        let root = build_above(bump, merged.nodes.into_vec());
        Bindings::new_tree(bump, pos, old_len + merged.added, root)
    }
}

/// Whether merging `small` attributes into a tree of `big` by path copying
/// beats a full rebuild: a leaf rebuild costs about `LEAF_MAX` copies per
/// touched leaf, so it pays while the small side touches a minority of them.
fn path_copy_pays(big: usize, small: usize) -> bool {
    small * LEAF_MAX * 2 < big
}

/// `left // right` over the two sets' attributes as slices: one pass
/// counts the result, a second writes it straight into its slice (or its
/// tree's leaves).
fn merge_slices<'a>(
    bump: &'a Bump,
    pos: SrcPos,
    left: &[Attr<'a>],
    right: &[Attr<'a>],
) -> Bindings<'a> {
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        let (x, y) = (left[i].name, right[j].name);
        i += usize::from(x <= y);
        j += usize::from(y <= x);
        shared += usize::from(x == y);
    }
    let len = left.len() + right.len() - shared;
    let (mut i, mut j) = (0, 0);
    let merged = Bindings::from_fn(bump, len, pos, |_| {
        if j < right.len() && (i == left.len() || right[j].name <= left[i].name) {
            // `right` wins on a shared name.
            if i < left.len() && left[i].name == right[j].name {
                i += 1;
            }
            j += 1;
            right[j - 1]
        } else {
            i += 1;
            left[i - 1]
        }
    });
    debug_assert!(merged.is_canonical());
    merged
}

/// Appends the attributes under `node` to `out`, in order.
fn collect_leaves<'a>(node: &'a Node<'a>, out: &mut Vec<Attr<'a>>) {
    match node {
        Node::Leaf(attrs) => out.extend_from_slice(attrs),
        Node::Inner { children, .. } => {
            for child in children.iter() {
                collect_leaves(child, out);
            }
        }
    }
}

/// Splits `len` items into the fewest chunks of at most `max`, as evenly
/// as possible, so that no chunk is smaller than half of `max` (when
/// `len > max`).
fn even_chunks(len: usize, max: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    let chunks = len.div_ceil(max).max(1);
    let base = len / chunks;
    let extra = len % chunks;
    let mut start = 0;
    (0..chunks).map(move |i| {
        let size = base + usize::from(i < extra);
        let range = start..start + size;
        start += size;
        range
    })
}

/// Builds the inner levels over `nodes` (siblings in order) up to a single
/// root.
fn build_above<'a>(bump: &'a Bump, mut nodes: Vec<&'a Node<'a>>) -> &'a Node<'a> {
    while nodes.len() > 1 {
        nodes = even_chunks(nodes.len(), INNER_MAX)
            .map(|range| inner_node(bump, &nodes[range]))
            .collect();
    }
    nodes[0]
}

fn inner_node<'a>(bump: &'a Bump, children: &[&'a Node<'a>]) -> &'a Node<'a> {
    debug_assert!(!children.is_empty());
    let firsts: &'a [Symbol] = bump.alloc_slice_fill_iter(children.iter().map(|c| c.first()));
    bump.alloc(Node::Inner {
        children: bump.alloc_slice_copy(children),
        firsts,
    })
}

/// The replacement for a node after a bulk insertion: one node, or several
/// siblings when it had to split, plus how many names were new.
struct Merged<'a> {
    nodes: SmallVec<[&'a Node<'a>; 4]>,
    added: usize,
    /// Attributes copied into new leaves.
    written: usize,
}

/// Inserts `items` (canonically ordered, unique) into the subtree `node`.
/// With `replace`, an item displaces an existing attribute of the same
/// name; otherwise the existing one stays.
fn insert_all<'a>(
    bump: &'a Bump,
    node: &'a Node<'a>,
    items: &[Attr<'a>],
    replace: bool,
) -> Merged<'a> {
    match node {
        Node::Leaf(attrs) => {
            let mut merged: Vec<Attr<'a>> = Vec::with_capacity(attrs.len() + items.len());
            let (mut i, mut j) = (0, 0);
            while i < attrs.len() && j < items.len() {
                match attrs[i].name.cmp(&items[j].name) {
                    Ordering::Equal => {
                        merged.push(if replace { items[j] } else { attrs[i] });
                        i += 1;
                        j += 1;
                    }
                    Ordering::Less => {
                        merged.push(attrs[i]);
                        i += 1;
                    }
                    Ordering::Greater => {
                        merged.push(items[j]);
                        j += 1;
                    }
                }
            }
            merged.extend_from_slice(&attrs[i..]);
            merged.extend_from_slice(&items[j..]);
            let added = merged.len() - attrs.len();
            let written = merged.len();
            let slice: &'a [Attr<'a>] = bump.alloc_slice_copy(&merged);
            let nodes = even_chunks(slice.len(), LEAF_MAX)
                .map(|range| &*bump.alloc(Node::Leaf(&slice[range])))
                .collect();
            Merged {
                nodes,
                added,
                written,
            }
        }
        Node::Inner { children, firsts } => {
            let mut new_children: Vec<&'a Node<'a>> = Vec::with_capacity(children.len() + 1);
            let mut added = 0;
            let mut written = 0;
            let mut start = 0;
            for (i, child) in children.iter().enumerate() {
                let end = match firsts.get(i + 1) {
                    Some(&next_first) => {
                        start + items[start..].partition_point(|it| it.name < next_first)
                    }
                    None => items.len(),
                };
                if start < end {
                    let merged = insert_all(bump, child, &items[start..end], replace);
                    added += merged.added;
                    written += merged.written;
                    new_children.extend(merged.nodes);
                } else {
                    new_children.push(child);
                }
                start = end;
            }
            let nodes = even_chunks(new_children.len(), INNER_MAX)
                .map(|range| inner_node(bump, &new_children[range]))
                .collect();
            Merged {
                nodes,
                added,
                written,
            }
        }
    }
}

/// The attributes of `attrs` whose names are not in `names` (both in
/// canonical order).
fn filter_out<'a>(attrs: &[Attr<'a>], names: &[Symbol]) -> Vec<Attr<'a>> {
    let mut kept = Vec::with_capacity(attrs.len());
    let mut j = 0;
    for a in attrs {
        while j < names.len() && names[j] < a.name {
            j += 1;
        }
        if j < names.len() && names[j] == a.name {
            j += 1;
            continue;
        }
        kept.push(*a);
    }
    kept
}

/// Removes `names` (canonically ordered) from the subtree. Returns the
/// replacement node (the same node when nothing changed, `None` when it
/// became empty) and the number of attributes removed.
fn remove_all<'a>(
    bump: &'a Bump,
    node: &'a Node<'a>,
    names: &[Symbol],
) -> (Option<&'a Node<'a>>, usize) {
    match node {
        Node::Leaf(attrs) => {
            let kept = filter_out(attrs, names);
            let removed = attrs.len() - kept.len();
            if removed == 0 {
                (Some(node), 0)
            } else if kept.is_empty() {
                (None, removed)
            } else {
                (
                    Some(bump.alloc(Node::Leaf(bump.alloc_slice_copy(&kept)))),
                    removed,
                )
            }
        }
        Node::Inner { children, firsts } => {
            let mut new_children: Vec<&'a Node<'a>> = Vec::with_capacity(children.len());
            let mut removed = 0;
            let mut changed = false;
            let mut start = 0;
            for (i, child) in children.iter().enumerate() {
                let end = match firsts.get(i + 1) {
                    Some(&next_first) => {
                        start + names[start..].partition_point(|&n| n < next_first)
                    }
                    None => names.len(),
                };
                if start < end {
                    let (replacement, r) = remove_all(bump, child, &names[start..end]);
                    removed += r;
                    match replacement {
                        Some(n) => {
                            changed |= !std::ptr::eq(n, *child);
                            new_children.push(n);
                        }
                        None => changed = true,
                    }
                } else {
                    new_children.push(child);
                }
                start = end;
            }
            if !changed {
                (Some(node), removed)
            } else if new_children.is_empty() {
                (None, removed)
            } else if new_children.len() == 1 {
                (Some(new_children[0]), removed)
            } else {
                (Some(inner_node(bump, &new_children)), removed)
            }
        }
    }
}

/// In-order iteration over a set.
pub struct Iter<'a> {
    /// Inner nodes on the path to the current leaf, with the index of the
    /// child to visit next.
    stack: SmallVec<[(&'a Node<'a>, usize); 6]>,
    leaf: std::slice::Iter<'a, Attr<'a>>,
    remaining: usize,
}

impl<'a> Iter<'a> {
    fn over(root: &'a Node<'a>, len: usize) -> Iter<'a> {
        let mut it = Iter {
            stack: SmallVec::new(),
            leaf: [].iter(),
            remaining: len,
        };
        it.descend(root);
        it
    }

    /// Makes the leftmost leaf under `node` the current leaf.
    fn descend(&mut self, mut node: &'a Node<'a>) {
        loop {
            match node {
                Node::Leaf(attrs) => {
                    self.leaf = attrs.iter();
                    return;
                }
                Node::Inner { children, .. } => {
                    self.stack.push((node, 1));
                    node = children[0];
                }
            }
        }
    }

    /// Moves to the next leaf, if any.
    fn advance(&mut self) -> bool {
        while let Some((node, next)) = self.stack.last_mut() {
            let Node::Inner { children, .. } = node else {
                unreachable!()
            };
            if *next < children.len() {
                let child = children[*next];
                *next += 1;
                self.descend(child);
                return true;
            }
            self.stack.pop();
        }
        false
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Attr<'a>;

    fn next(&mut self) -> Option<&'a Attr<'a>> {
        loop {
            if let Some(a) = self.leaf.next() {
                self.remaining -= 1;
                return Some(a);
            }
            if !self.advance() {
                return None;
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Iter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolTable;
    use crate::value::{Value, ValueCell};
    use std::collections::BTreeMap;

    /// The test names `k00000000`, `k00000001`, …: zero-padded, so name
    /// order is numeric order, and nine characters long, so the eight-byte
    /// comparison prefix alone cannot tell neighbours apart.
    const UNIVERSE: usize = 20_001;
    /// Coprime to `UNIVERSE`: the fixture interns name `i * STRIDE %
    /// UNIVERSE` `i`-th, so symbol-id order (the sets' order) is a
    /// scrambled permutation of name order, and every test that compares
    /// against name order would catch the two being confused.
    const STRIDE: usize = 7919;

    struct Fixture<'a> {
        bump: &'a Bump,
        keys: &'a SymbolKeys<'a>,
        table: SymbolTable<'a>,
        cell: ValueRef<'a>,
        /// `syms[i]` is the symbol of name number `i`.
        syms: Vec<Symbol>,
    }

    impl<'a> Fixture<'a> {
        fn new(bump: &'a Bump) -> Fixture<'a> {
            let keys = SymbolKeys::new(bump);
            let mut table = SymbolTable::new(keys);
            let mut syms = vec![Symbol(u32::MAX); UNIVERSE];
            for k in 0..UNIVERSE {
                let i = k * STRIDE % UNIVERSE;
                syms[i] = table.intern(Self::name(i).as_bytes());
            }
            Fixture {
                bump,
                keys,
                table,
                cell: bump.alloc(ValueCell::new(Value::Null)),
                syms,
            }
        }

        fn name(i: usize) -> String {
            format!("k{i:08}")
        }

        fn sym(&self, i: usize) -> Symbol {
            self.syms[i]
        }

        /// The number of a test name.
        fn id(&self, s: Symbol) -> usize {
            std::str::from_utf8(&self.keys.name(s)[1..])
                .unwrap()
                .parse()
                .unwrap()
        }

        fn attr(&self, i: usize, pos: u32) -> Attr<'a> {
            Attr {
                name: self.sym(i),
                pos: SrcPos(pos),
                value: self.cell,
            }
        }

        /// A set over `ids`, each attribute's position being its id.
        fn set(&self, ids: impl IntoIterator<Item = usize>) -> Bindings<'a> {
            self.set_at(ids, SrcPos::NONE)
        }

        fn set_at(&self, ids: impl IntoIterator<Item = usize>, pos: SrcPos) -> Bindings<'a> {
            let mut attrs: Vec<Attr<'a>> =
                ids.into_iter().map(|i| self.attr(i, i as u32)).collect();
            Bindings::sort_dedup(&mut attrs);
            Bindings::from_sorted(self.bump, &attrs, pos)
        }

        /// The names of a set, as numbers, in iteration order.
        fn ids(&self, b: Bindings<'a>) -> Vec<usize> {
            b.iter().map(|a| self.id(a.name)).collect()
        }

        /// `ids` (deduplicated) in the order a set keeps them: by symbol.
        fn in_set_order(&self, ids: impl IntoIterator<Item = usize>) -> Vec<usize> {
            let mut v: Vec<usize> = ids.into_iter().collect();
            v.sort_by_key(|&i| self.sym(i));
            v.dedup();
            v
        }

        fn update(&self, left: Bindings<'a>, right: Bindings<'a>) -> Bindings<'a> {
            Bindings::update(self.bump, left, right)
        }

        /// `removeAttrs` of the names with these ids.
        fn remove(&self, set: Bindings<'a>, ids: impl IntoIterator<Item = usize>) -> Bindings<'a> {
            let names: Vec<Symbol> = ids.into_iter().map(|i| self.sym(i)).collect();
            Bindings::remove(self.bump, set, &names)
        }
    }

    /// The set holds exactly `model` (name number → position): in symbol
    /// order when iterated, in name order through `lexicographic`, and
    /// every name is found by `get`.
    fn check_model<'a>(f: &Fixture<'a>, b: Bindings<'a>, model: &BTreeMap<usize, u32>) {
        assert_eq!(b.len(), model.len());
        assert_tree_invariants(b);
        assert_eq!(b.is_tree(), model.len() > FLAT_MAX);
        assert!(b.is_canonical());
        let got: Vec<(usize, u32)> = b.iter().map(|a| (f.id(a.name), a.pos.0)).collect();
        let mut want: Vec<(usize, u32)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        want.sort_by_key(|&(k, _)| f.sym(k));
        assert_eq!(got, want);
        assert_eq!(b.iter().len(), model.len());
        let lex: Vec<(usize, u32)> = b
            .lexicographic(f.keys)
            .iter()
            .map(|a| (f.id(a.name), a.pos.0))
            .collect();
        let by_name: Vec<(usize, u32)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(lex, by_name);
        for (&k, &v) in model {
            let a = b.get(f.sym(k)).unwrap_or_else(|| panic!("missing {k}"));
            assert_eq!(a.pos.0, v);
        }
    }

    /// The representation invariants of the module docs: a flat set holds
    /// at most `FLAT_MAX` attributes in strictly ascending symbol order;
    /// a tree holds more, every leaf is non-empty and at most `LEAF_MAX`
    /// long, every inner node has between two and `INNER_MAX` children
    /// (removal collapses one with a single child into it) and its
    /// `firsts[i]` is the first name under `children[i]`, names ascend
    /// across the whole tree, and the header's length is the count.
    /// Returns the tree's height (0 for a flat set, 1 for a lone leaf).
    fn assert_tree_invariants(b: Bindings<'_>) -> usize {
        match b.repr() {
            Repr::Flat(attrs) => {
                assert!(attrs.len() <= FLAT_MAX, "a flat set of {}", attrs.len());
                assert!(
                    attrs.windows(2).all(|w| w[0].name < w[1].name),
                    "flat attributes ascend"
                );
                0
            }
            Repr::Tree { root, len } => {
                assert!(len as usize > FLAT_MAX, "a tree of {len} attributes");
                let mut prev = None;
                let (count, height) = check_node(root, &mut prev, None);
                assert_eq!(count, len as usize, "the header counts the attributes");
                height
            }
        }
    }

    /// Checks the subtree `node`, whose names must all be above `prev`
    /// (the last name before it) and below `upper` (the first name after
    /// it, if any). Returns its attribute count and height.
    fn check_node(
        node: &Node<'_>,
        prev: &mut Option<Symbol>,
        upper: Option<Symbol>,
    ) -> (usize, usize) {
        match node {
            Node::Leaf(attrs) => {
                assert!(!attrs.is_empty(), "every leaf is non-empty");
                assert!(attrs.len() <= LEAF_MAX, "a leaf of {}", attrs.len());
                for a in attrs.iter() {
                    assert!(prev.is_none_or(|p| p < a.name), "names ascend");
                    assert!(upper.is_none_or(|u| a.name < u), "a name past its subtree");
                    *prev = Some(a.name);
                }
                (attrs.len(), 1)
            }
            Node::Inner { children, firsts } => {
                assert!(
                    (2..=INNER_MAX).contains(&children.len()),
                    "an inner node of {} children",
                    children.len()
                );
                assert_eq!(firsts.len(), children.len());
                let (mut count, mut height) = (0, 0);
                for (i, child) in children.iter().enumerate() {
                    assert_eq!(firsts[i], child.first(), "firsts[{i}] is its child's first");
                    let bound = firsts.get(i + 1).copied().or(upper);
                    let (c, h) = check_node(child, prev, bound);
                    count += c;
                    height = height.max(h);
                }
                (count, height + 1)
            }
        }
    }

    /// The leaves of a tree, as node addresses.
    fn leaf_addrs(b: Bindings<'_>) -> std::collections::HashSet<usize> {
        fn walk(node: &Node<'_>, out: &mut std::collections::HashSet<usize>) {
            match node {
                Node::Leaf(_) => {
                    out.insert(node as *const Node<'_> as usize);
                }
                Node::Inner { children, .. } => children.iter().for_each(|c| walk(c, out)),
            }
        }
        let mut out = std::collections::HashSet::new();
        if let Repr::Tree { root, .. } = b.repr() {
            walk(root, &mut out);
        }
        out
    }

    /// The attributes of `merged` in leaves that are not leaves of either
    /// operand: what a tree merge copied.
    fn attrs_in_new_leaves(merged: Bindings<'_>, left: Bindings<'_>, right: Bindings<'_>) -> usize {
        fn walk(node: &Node<'_>, old: &std::collections::HashSet<usize>) -> usize {
            match node {
                Node::Leaf(attrs) if !old.contains(&(node as *const Node<'_> as usize)) => {
                    attrs.len()
                }
                Node::Leaf(_) => 0,
                Node::Inner { children, .. } => children.iter().map(|c| walk(c, old)).sum(),
            }
        }
        let mut old = leaf_addrs(left);
        old.extend(leaf_addrs(right));
        match merged.repr() {
            Repr::Tree { root, .. } => walk(root, &old),
            Repr::Flat(attrs) => attrs.len(),
        }
    }

    /// Lookups of every member, and of names below, between and above the
    /// members' symbols.
    fn check_lookups<'a>(f: &mut Fixture<'a>, b: Bindings<'a>, ids: &[usize]) {
        for &i in ids {
            assert_eq!(
                b.get(f.sym(i)).map(|a| a.pos.0),
                Some(i as u32),
                "{} {i}",
                b.len()
            );
        }
        for i in 0..UNIVERSE {
            if !ids.contains(&i) && i % 97 == 0 {
                assert!(b.get(f.sym(i)).is_none(), "{i} is not a member");
            }
        }
        // The first static symbol is below every test name, a fresh one
        // above them all.
        assert!(b.get(Symbol(0)).is_none());
        let above = f.table.intern(b"a fresh name");
        assert!(b.get(above).is_none());
    }

    #[test]
    fn flat_and_tree_lookup_and_iteration() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        for n in [0, 1, 2, 7, 8, 9, 63, 64, 65, 66, 128, 129, 200, 5000] {
            let b = f.set(0..n);
            assert_eq!(b.len(), n);
            assert_eq!(b.is_empty(), n == 0);
            assert_eq!(b.is_tree(), n > FLAT_MAX);
            assert_eq!(b.as_flat().is_some(), n <= FLAT_MAX);
            assert!(b.is_canonical());
            assert_eq!(f.ids(b), f.in_set_order(0..n));
            assert_eq!(b.iter().len(), n);
            let lex: Vec<usize> = b
                .lexicographic(f.keys)
                .iter()
                .map(|a| f.id(a.name))
                .collect();
            assert_eq!(lex, (0..n).collect::<Vec<_>>());
            check_lookups(&mut f, b, &(0..n).collect::<Vec<_>>());
        }
    }

    /// The scrambled fixture really does order sets differently from
    /// names, so the other tests tell the two orders apart.
    #[test]
    fn the_fixture_scrambles_symbol_order() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let b = f.set(0..100);
        assert_ne!(f.ids(b), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn empty_set() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let e = Bindings::empty(&bump, SrcPos(7));
        assert_eq!(e.len(), 0);
        assert!(e.is_empty());
        assert!(!e.is_tree());
        assert_eq!(e.pos(), SrcPos(7));
        assert_eq!(e.iter().next().map(|a| a.name), None);
        assert_eq!(e.as_flat().map(<[_]>::len), Some(0));
        assert!(e.get(f.sym(0)).is_none());
        assert!(e.lexicographic(f.keys).is_empty());
        let from_nothing = Bindings::from_sorted(&bump, &[], SrcPos::NONE);
        assert!(from_nothing.is_empty());
        let mapped = Bindings::map(&bump, e, SrcPos::NONE, |a| *a);
        assert!(mapped.is_empty());
        assert!(f.remove(e, [1]).is_empty());
    }

    #[test]
    fn one_attribute() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        let one = f.set_at([42], SrcPos(3));
        assert_eq!(one.len(), 1);
        assert_eq!(one.pos(), SrcPos(3));
        assert_eq!(f.ids(one), vec![42]);
        check_lookups(&mut f, one, &[42]);
        let other = f.set([7]);
        let both = f.update(one, other);
        assert_eq!(f.ids(both), f.in_set_order([7, 42]));
        assert_eq!(
            both.pos(),
            SrcPos::NONE,
            "`//` makes a set without a position"
        );
        let again = f.set([42]);
        let replaced = f.update(one, again);
        assert_eq!(replaced.len(), 1);
        assert!(f.remove(one, [42]).is_empty());
    }

    /// The handle is one pointer, and a set's identity is its header's:
    /// copies of a handle are the same set, equal sets built separately
    /// are not, whatever their shape.
    #[test]
    fn handles_are_one_pointer_with_header_identity() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        assert_eq!(
            std::mem::size_of::<Bindings<'_>>(),
            std::mem::size_of::<usize>()
        );
        assert_eq!(
            std::mem::size_of::<Option<Bindings<'_>>>(),
            std::mem::size_of::<usize>()
        );
        for n in [0, 1, 5, FLAT_MAX, FLAT_MAX + 1, 300] {
            let a = f.set(0..n);
            let copy = a;
            assert_eq!(copy.addr(), a.addr(), "{n}");
            assert_ne!(f.set(0..n).addr(), a.addr(), "{n}");
        }
        // Two empty sets are still two sets.
        let e1 = Bindings::empty(&bump, SrcPos::NONE);
        let e2 = Bindings::empty(&bump, SrcPos::NONE);
        assert_ne!(e1.addr(), e2.addr());
    }

    /// `from_fn` asks for each attribute once, in index order, for both
    /// shapes.
    #[test]
    fn from_fn_calls_in_index_order() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        for n in [0, 3, FLAT_MAX, FLAT_MAX + 1, 200] {
            let ids = f.in_set_order(0..n);
            let mut calls = Vec::new();
            let b = Bindings::from_fn(&bump, n, SrcPos(9), |k| {
                calls.push(k);
                f.attr(ids[k], ids[k] as u32)
            });
            assert_eq!(calls, (0..n).collect::<Vec<_>>());
            assert_eq!(b.len(), n);
            assert_eq!(b.pos(), SrcPos(9));
            assert_eq!(f.ids(b), ids);
        }
    }

    /// Every operation crossing the flat/tree boundary in either direction.
    #[test]
    fn the_flat_tree_boundary() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        // One more attribute makes a tree, from either side of `//`,
        // whether its symbol sorts first, last or in between.
        let mut by_symbol = f.in_set_order(0..FLAT_MAX + 3);
        let candidates = [
            by_symbol.remove(0),
            by_symbol.pop().unwrap(),
            by_symbol.remove(10),
        ];
        let base: Vec<usize> = by_symbol[..FLAT_MAX].to_vec();
        let full = f.set(base.iter().copied());
        assert_eq!(full.len(), FLAT_MAX);
        assert!(!full.is_tree());
        for extra_id in candidates {
            let extra = f.set([extra_id]);
            for grown in [f.update(full, extra), f.update(extra, full)] {
                assert!(grown.is_tree());
                let all: Vec<usize> = base.iter().copied().chain([extra_id]).collect();
                assert_eq!(f.ids(grown), f.in_set_order(all.iter().copied()));
                check_lookups(&mut f, grown, &all);
            }
        }
        // Overlapping names keep it at the limit and flat.
        let overlapping = f.set([base[0], base[FLAT_MAX - 1]]);
        let same = f.update(full, overlapping);
        assert!(!same.is_tree());
        assert_eq!(same.len(), FLAT_MAX);
        // Removing one attribute from the smallest tree flattens it.
        let tree = f.set(0..=FLAT_MAX);
        assert!(tree.is_tree());
        let flat = f.remove(tree, [10]);
        assert!(!flat.is_tree());
        assert_eq!(flat.len(), FLAT_MAX);
        assert!(flat.get(f.sym(10)).is_none());
        check_lookups(
            &mut f,
            flat,
            &(0..=FLAT_MAX).filter(|&i| i != 10).collect::<Vec<_>>(),
        );
        // Removing nothing keeps the shape.
        let kept = f.remove(tree, [9999]);
        assert!(kept.is_tree());
        assert_eq!(kept.len(), FLAT_MAX + 1);
        // `map` keeps the shape on both sides of the boundary.
        for b in [full, tree] {
            let mapped = Bindings::map(&bump, b, SrcPos(1), |a| Attr {
                name: a.name,
                pos: SrcPos(a.pos.0 + 1000),
                value: a.value,
            });
            assert_eq!(mapped.is_tree(), b.is_tree());
            assert_eq!(mapped.pos(), SrcPos(1));
            assert!(mapped
                .iter()
                .zip(b.iter())
                .all(|(m, o)| m.name == o.name && m.pos.0 == o.pos.0 + 1000));
        }
    }

    /// `//` between every combination of flat and tree operands, against a
    /// model, including the path-copying merges into a large tree.
    #[test]
    fn update_across_shapes() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let shapes: [(&str, Vec<usize>); 6] = [
            ("small flat", vec![1, 5, 9]),
            ("full flat", (0..FLAT_MAX).map(|i| i * 3).collect()),
            ("smallest tree", (0..FLAT_MAX + 1).map(|i| i * 2).collect()),
            ("small tree", (0..FLAT_MAX + 2).map(|i| i * 5).collect()),
            ("tree", (0..500).collect()),
            ("big tree", (0..20_000).step_by(3).collect()),
        ];
        for (ln, l) in &shapes {
            for (rn, r) in &shapes {
                let left = f.set_at(l.iter().copied(), SrcPos(11));
                let mut right_attrs: Vec<Attr<'_>> =
                    r.iter().map(|&i| f.attr(i, 1_000_000 + i as u32)).collect();
                Bindings::sort_dedup(&mut right_attrs);
                let right = Bindings::from_sorted(&bump, &right_attrs, SrcPos(22));
                let merged = f.update(left, right);
                let mut model: BTreeMap<usize, u32> = l.iter().map(|&i| (i, i as u32)).collect();
                model.extend(r.iter().map(|&i| (i, 1_000_000 + i as u32)));
                check_model(&f, merged, &model);
                assert_eq!(merged.pos(), SrcPos::NONE, "{ln} // {rn}");
                // The operands are untouched.
                assert_eq!(left.len(), l.len());
                assert_eq!(f.ids(left), f.in_set_order(l.iter().copied()));
            }
        }
    }

    /// Name order is byte order, also when names share the eight-byte
    /// comparison prefix or are prefixes of each other, and whatever order
    /// their symbols were created in.
    #[test]
    fn lexicographic_order_with_prefix_ties() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        // All share the eight-byte prefix "propagat", and are created out
        // of name order.
        let names: [&[u8]; 5] = [
            b"propagatedBuildInputs",
            b"propagatedNativeBuildInputs",
            b"propagat",
            b"propagated",
            b"propagatedBuildInputs2",
        ];
        let mut attrs: Vec<Attr<'_>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| Attr::new(f.table.intern(n), SrcPos(i as u32), f.cell))
            .collect();
        assert!(attrs
            .iter()
            .all(|a| f.keys.prefix(a.name) == f.keys.prefix(attrs[0].name)));
        Bindings::sort_dedup(&mut attrs);
        let b = Bindings::from_sorted(&bump, &attrs, SrcPos::NONE);
        let iterated: Vec<&[u8]> = b.iter().map(|a| f.keys.name(a.name)).collect();
        assert_eq!(iterated, names.to_vec(), "iteration is creation order");
        let lex: Vec<&[u8]> = b
            .lexicographic(f.keys)
            .iter()
            .map(|a| f.keys.name(a.name))
            .collect();
        let mut expected = names.to_vec();
        expected.sort();
        assert_eq!(lex, expected);
        for (i, n) in names.iter().enumerate() {
            let s = f.table.get(n).unwrap();
            assert_eq!(b.get(s).map(|a| a.pos.0), Some(i as u32));
        }
        // A tree of such names, created in reverse name order.
        let mut many: Vec<Attr<'_>> = (0..300u32)
            .rev()
            .map(|i| {
                let s = f.table.intern(format!("propagat{i:04}").as_bytes());
                Attr::new(s, SrcPos(i), f.cell)
            })
            .collect();
        Bindings::sort_dedup(&mut many);
        let t = Bindings::from_sorted(&bump, &many, SrcPos::NONE);
        assert!(t.is_tree());
        let iterated: Vec<u32> = t.iter().map(|a| a.pos.0).collect();
        assert_eq!(iterated, (0..300).rev().collect::<Vec<_>>());
        let lex: Vec<u32> = t.lexicographic(f.keys).iter().map(|a| a.pos.0).collect();
        assert_eq!(lex, (0..300).collect::<Vec<_>>());
        for i in 0..300u32 {
            let s = f.table.get(format!("propagat{i:04}").as_bytes()).unwrap();
            assert_eq!(t.get(s).map(|a| a.pos.0), Some(i));
        }
    }

    /// `//` of every pair of non-empty subsets of eight names, so every
    /// interleaving of two small flat sets is merged once.
    #[test]
    fn every_small_flat_merge() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let subset = |bits: u32| (0..8).filter(move |i| bits & (1 << i) != 0);
        for lb in 1..256u32 {
            for rb in 1..256u32 {
                let left = f.set(subset(lb));
                let mut right_attrs: Vec<Attr<'_>> =
                    subset(rb).map(|i| f.attr(i, 100 + i as u32)).collect();
                Bindings::sort_dedup(&mut right_attrs);
                let right = Bindings::from_sorted(&bump, &right_attrs, SrcPos::NONE);
                let mut model: BTreeMap<usize, u32> = subset(lb).map(|i| (i, i as u32)).collect();
                model.extend(subset(rb).map(|i| (i, 100 + i as u32)));
                let (merged, written) = Bindings::update_counted(&bump, left, right);
                check_model(&f, merged, &model);
                assert_eq!(written, model.len());
            }
        }
    }

    #[test]
    fn update_right_bias_and_sharing() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let big = f.set(0..3000);
        let mut small_attrs = vec![f.attr(5, 999), f.attr(2999, 998), f.attr(4000, 997)];
        Bindings::sort_dedup(&mut small_attrs);
        let small = Bindings::from_sorted(&bump, &small_attrs, SrcPos::NONE);
        let merged = f.update(big, small);
        assert_eq!(merged.len(), 3001);
        assert_eq!(merged.get(f.sym(5)).unwrap().pos.0, 999);
        assert_eq!(merged.get(f.sym(2999)).unwrap().pos.0, 998);
        assert_eq!(merged.get(f.sym(4000)).unwrap().pos.0, 997);
        assert_eq!(merged.get(f.sym(6)).unwrap().pos.0, 6);
        // The original is untouched.
        assert_eq!(big.get(f.sym(5)).unwrap().pos.0, 5);
        assert_eq!(big.len(), 3000);
        // Left wins nowhere: small // big keeps big's values.
        let merged2 = f.update(small, big);
        assert_eq!(merged2.len(), 3001);
        assert_eq!(merged2.get(f.sym(5)).unwrap().pos.0, 5);
        assert_eq!(merged2.get(f.sym(4000)).unwrap().pos.0, 997);
        assert_eq!(f.ids(merged2), f.in_set_order((0..3000).chain([4000])));
    }

    #[test]
    fn remove_and_flatten() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let big = f.set_at(0..300, SrcPos(5));
        let small = f.remove(big, 0..250);
        assert_eq!(small.len(), 50);
        assert!(!small.is_tree());
        assert_eq!(
            small.pos(),
            SrcPos::NONE,
            "removeAttrs makes a set without a position"
        );
        assert_eq!(f.ids(small), f.in_set_order(250..300));
        let none = f.remove(big, 0..300);
        assert!(none.is_empty());
        let same = f.remove(big, [9999]);
        assert_eq!(same.len(), 300);
        assert!(same.is_tree());
        assert_eq!(same.pos(), SrcPos::NONE);
        // Duplicates and names in any order.
        let some = f.remove(big, [299, 3, 299, 150, 3]);
        assert_eq!(
            f.ids(some),
            f.in_set_order((0..300).filter(|i| ![3, 150, 299].contains(i)))
        );
    }

    #[test]
    fn iteration_is_creation_order_and_lexicographic_is_name_order() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        let z = f.table.intern(b"z");
        let a = f.table.intern(b"a");
        let mut attrs = vec![
            Attr {
                name: a,
                pos: SrcPos(2),
                value: f.cell,
            },
            Attr {
                name: z,
                pos: SrcPos(1),
                value: f.cell,
            },
        ];
        Bindings::sort_dedup(&mut attrs);
        let b = Bindings::from_sorted(&bump, &attrs, SrcPos::NONE);
        assert_eq!(b.iter().map(|x| x.name).collect::<Vec<_>>(), vec![z, a]);
        assert_eq!(
            b.lexicographic(f.keys)
                .iter()
                .map(|x| x.name)
                .collect::<Vec<_>>(),
            vec![a, z]
        );
    }

    #[test]
    fn sort_dedup_keeps_the_later_duplicate() {
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let mut attrs = vec![
            f.attr(3, 1),
            f.attr(1, 2),
            f.attr(3, 3),
            f.attr(2, 4),
            f.attr(1, 5),
        ];
        Bindings::sort_dedup(&mut attrs);
        let got: Vec<(usize, u32)> = attrs.iter().map(|a| (f.id(a.name), a.pos.0)).collect();
        let mut want = vec![(1, 5), (2, 4), (3, 3)];
        want.sort_by_key(|&(i, _)| f.sym(i));
        assert_eq!(got, want);
    }

    /// The attribute numbers in a leaf.
    fn leaf_ids(f: &Fixture<'_>, node: &Node<'_>) -> Vec<usize> {
        match node {
            Node::Leaf(attrs) => attrs.iter().map(|a| f.id(a.name)).collect(),
            Node::Inner { .. } => panic!("expected a leaf"),
        }
    }

    /// `left // right` against the model, checking how many attributes
    /// the merge reports as written: those in leaves it made.
    fn update_checked<'a>(
        f: &Fixture<'a>,
        left: Bindings<'a>,
        right: Bindings<'a>,
        model: &BTreeMap<usize, u32>,
    ) -> Bindings<'a> {
        let (merged, written) = Bindings::update_counted(f.bump, left, right);
        check_model(f, merged, model);
        assert_eq!(written, attrs_in_new_leaves(merged, left, right));
        merged
    }

    /// A 20,000-attribute set is three levels deep. Removing all but the
    /// first leaf of one subtree below the root collapses that subtree
    /// into the leaf, which then sits one level higher than its cousins;
    /// lookups, iteration, `//` (both ways) and further removals must all
    /// still work on the uneven tree.
    #[test]
    fn removal_collapses_a_subtree_into_its_last_leaf() {
        let bump = Bump::new();
        let mut f = Fixture::new(&bump);
        let big = f.set(0..20_000);
        assert_eq!(assert_tree_invariants(big), 3);
        let Repr::Tree { root, .. } = big.repr() else {
            panic!("20,000 attributes are a tree")
        };
        let Node::Inner { children, .. } = root else {
            panic!("the root of a three-level tree is an inner node")
        };
        let Node::Inner {
            children: leaves, ..
        } = children[1]
        else {
            panic!("the root's children are inner nodes")
        };
        let kept = leaf_ids(&f, leaves[0]);
        let removed: Vec<usize> = leaves[1..].iter().flat_map(|l| leaf_ids(&f, l)).collect();
        let mut model: BTreeMap<usize, u32> = (0..20_000).map(|i| (i, i as u32)).collect();
        for i in &removed {
            model.remove(i);
        }
        let collapsed = f.remove(big, removed.iter().copied());
        check_model(&f, collapsed, &model);
        let Repr::Tree { root, .. } = collapsed.repr() else {
            panic!("still a tree")
        };
        let Node::Inner { children, .. } = root else {
            panic!("the root is still an inner node")
        };
        assert_eq!(leaf_ids(&f, children[1]), kept, "the subtree is its leaf");

        // Names in the collapsed leaf, in removed ones, at both ends and
        // past the end, merged in from either side.
        let touched = [
            kept[0],
            kept[kept.len() - 1],
            removed[0],
            removed[removed.len() / 2],
            0,
            19_999,
            20_000,
        ];
        let mut right_attrs: Vec<Attr<'_>> = touched
            .iter()
            .map(|&i| f.attr(i, 50_000 + i as u32))
            .collect();
        Bindings::sort_dedup(&mut right_attrs);
        let right = Bindings::from_sorted(&bump, &right_attrs, SrcPos::NONE);
        let mut right_wins = model.clone();
        right_wins.extend(touched.iter().map(|&i| (i, 50_000 + i as u32)));
        let merged = update_checked(&f, collapsed, right, &right_wins);
        let mut left_wins = model.clone();
        for &i in &touched {
            left_wins.entry(i).or_insert(50_000 + i as u32);
        }
        update_checked(&f, right, collapsed, &left_wins);
        let members: Vec<usize> = model.keys().copied().collect();
        check_lookups(&mut f, collapsed, &members);

        // Emptying the collapsed leaf removes it from the root.
        let emptied = f.remove(merged, kept.iter().copied());
        for i in &kept {
            right_wins.remove(i);
        }
        check_model(&f, emptied, &right_wins);
    }

    /// Small `//`s into a 4,000-attribute set, whose leaves start out
    /// full: leaves split on every insertion, until the root has more
    /// than `INNER_MAX` children and splits too, adding a level. Every
    /// step also checks the number of attributes the merge reports as
    /// written.
    #[test]
    fn small_updates_split_inner_nodes() {
        use rand::{RngExt, SeedableRng};
        use rand_pcg::Pcg64;
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let mut set = f.set((0..4000).map(|i| i * 5));
        let mut model: BTreeMap<usize, u32> = (0..4000).map(|i| (i * 5, (i * 5) as u32)).collect();
        assert_eq!(assert_tree_invariants(set), 2);
        let mut rng = Pcg64::seed_from_u64(0x5_9117);
        for round in 0..60u32 {
            let mut attrs: Vec<Attr<'_>> = (0..20)
                .map(|_| {
                    // Mostly new names, now and then an existing one.
                    let i = rng.random_range(0..20_000);
                    let v = 100_000 + round;
                    model.insert(i, v);
                    f.attr(i, v)
                })
                .collect();
            Bindings::sort_dedup(&mut attrs);
            let right = Bindings::from_sorted(&bump, &attrs, SrcPos::NONE);
            set = update_checked(&f, set, right, &model);
        }
        assert_eq!(assert_tree_invariants(set), 3, "the root split");
    }

    /// Random merges, removals and lookups against a `BTreeMap` model.
    #[test]
    fn random_operations_match_a_model() {
        use rand::{RngExt, SeedableRng};
        use rand_pcg::Pcg64;
        let bump = Bump::new();
        let f = Fixture::new(&bump);
        let mut rng = Pcg64::seed_from_u64(0x5eed);
        let mut version = 1u32;
        for round in 0..40 {
            let universe = [10usize, 100, 1000, 20_000][round % 4];
            let mut model: BTreeMap<usize, u32> = BTreeMap::new();
            let n = rng.random_range(0..=universe.min(3000));
            let mut attrs = Vec::new();
            for _ in 0..n {
                let k = rng.random_range(0..universe);
                version += 1;
                model.insert(k, version);
                attrs.push(f.attr(k, version));
            }
            Bindings::sort_dedup(&mut attrs);
            let mut set = Bindings::from_sorted(&bump, &attrs, SrcPos::NONE);
            check_model(&f, set, &model);
            for _ in 0..12 {
                match rng.random_range(0..4) {
                    0 | 1 => {
                        // Merge a set of random size (small, medium or huge).
                        let m = match rng.random_range(0..3) {
                            0 => rng.random_range(1..=5),
                            1 => rng.random_range(1..=200),
                            _ => rng.random_range(1..=3000),
                        };
                        let mut other_model = BTreeMap::new();
                        let mut other = Vec::new();
                        for _ in 0..m {
                            let k = rng.random_range(0..universe);
                            version += 1;
                            other_model.insert(k, version);
                            other.push(f.attr(k, version));
                        }
                        Bindings::sort_dedup(&mut other);
                        let other_set = Bindings::from_sorted(&bump, &other, SrcPos::NONE);
                        if set.is_empty() {
                            set = other_set;
                            model = other_model;
                        } else if !other_set.is_empty() {
                            if rng.random_bool(0.5) {
                                model.extend(other_model);
                                set = update_checked(&f, set, other_set, &model);
                            } else {
                                for (k, v) in other_model {
                                    model.entry(k).or_insert(v);
                                }
                                set = update_checked(&f, other_set, set, &model);
                            }
                        }
                    }
                    2 => {
                        // Up to every name of the set, mostly members.
                        let m = rng.random_range(0..=set.len());
                        let members: Vec<usize> = model.keys().copied().collect();
                        let ids: Vec<usize> = (0..m)
                            .map(|_| {
                                let k = if rng.random_bool(0.8) {
                                    members[rng.random_range(0..members.len())]
                                } else {
                                    rng.random_range(0..universe)
                                };
                                model.remove(&k);
                                k
                            })
                            .collect();
                        set = f.remove(set, ids);
                    }
                    _ => {
                        let mapped = Bindings::map(&bump, set, SrcPos::NONE, |a| Attr {
                            name: a.name,
                            pos: SrcPos(a.pos.0 + 1),
                            value: a.value,
                        });
                        for v in model.values_mut() {
                            *v += 1;
                        }
                        set = mapped;
                    }
                }
                check_model(&f, set, &model);
                for _ in 0..20 {
                    let k = rng.random_range(0..universe);
                    assert_eq!(set.get(f.sym(k)).map(|a| a.pos.0), model.get(&k).copied());
                }
            }
        }
    }
}
