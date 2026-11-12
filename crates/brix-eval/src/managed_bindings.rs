//! Persistent attribute sets on the moving heap.
//!
//! This preserves `bindings.rs`'s flat representation, 64-way persistent
//! B+ tree, and minority-side merge heuristic. A small update copies only
//! touched leaves and their ancestor paths. Node snapshots root their cells
//! and children before construction can collect; no borrowed attribute or
//! child slice survives an allocating call. Full merges instead collect once
//! before a private batch, rooting only the input owners and completed result.

#![allow(dead_code)] // Evaluator callers are migrated separately.

use std::cmp::Ordering;

use brix_gc::Result;

use crate::managed_heap::{
    BindingsContents, BindingsRoot, Heap, NodeContents, NodeRoot, RootedAttr, Roots,
};
use crate::symbol::{Symbol, SymbolKeys};
use crate::value::SrcPos;

pub(crate) use crate::bindings::FLAT_MAX;
const LEAF_MAX: usize = 64;
const INNER_MAX: usize = 64;

/// Stable sorting makes the later duplicate win, including its position.
pub(crate) fn sort_dedup(attrs: &mut Vec<RootedAttr<'_, '_>>) {
    attrs.sort_by_key(|attr| attr.name);
    attrs.dedup_by(|later, earlier| {
        if later.name == earlier.name {
            *earlier = *later;
            true
        } else {
            false
        }
    });
}

pub(crate) fn empty<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    pos: SrcPos,
) -> Result<BindingsRoot<'r, 'h>> {
    heap.alloc_flat_bindings(roots, pos, &[])
}

/// Builds from unique attributes in ascending symbol-id order.
pub(crate) fn from_sorted<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    attrs: &[RootedAttr<'_, 'h>],
    pos: SrcPos,
) -> Result<BindingsRoot<'r, 'h>> {
    debug_assert!(attrs.windows(2).all(|pair| pair[0].name < pair[1].name));
    // A flat constructor has no intermediate managed object to protect.
    if attrs.len() <= FLAT_MAX {
        return heap.alloc_flat_bindings(roots, pos, attrs);
    }
    heap.with_roots(|heap, temporary| {
        let result = build_sorted(heap, temporary, attrs, pos)?;
        Ok(roots.copy(result))
    })
}

/// The callback returns rooted attributes and may allocate or collect.
pub(crate) fn from_fn<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    len: usize,
    pos: SrcPos,
    mut attr: impl FnMut(&mut Heap<'h>, &Roots<'r, 'h>, usize) -> Result<RootedAttr<'r, 'h>>,
) -> Result<BindingsRoot<'r, 'h>> {
    let mut attrs = Vec::with_capacity(len);
    for index in 0..len {
        attrs.push(attr(heap, roots, index)?);
    }
    from_sorted(heap, roots, &attrs, pos)
}

/// `left // right`, preserving the other operand's identity for an empty side.
pub(crate) fn update<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    left: BindingsRoot<'_, 'h>,
    right: BindingsRoot<'_, 'h>,
) -> Result<BindingsRoot<'r, 'h>> {
    update_counted(heap, roots, left, right).map(|(result, _)| result)
}

/// Returns the result and the number of attributes copied into new leaves.
pub(crate) fn update_counted<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    left: BindingsRoot<'_, 'h>,
    right: BindingsRoot<'_, 'h>,
) -> Result<(BindingsRoot<'r, 'h>, usize)> {
    let left_len = heap.bindings_view(left).len();
    let right_len = heap.bindings_view(right).len();
    if left_len == 0 {
        return Ok((roots.copy(right), 0));
    }
    if right_len == 0 {
        return Ok((roots.copy(left), 0));
    }
    if left_len <= FLAT_MAX && right_len <= FLAT_MAX {
        if let Some(result) = heap.merge_flat_bindings(roots, left, right)? {
            return Ok((result, heap.bindings_view(result).len()));
        }
    }
    let copy_left_path = left_len > FLAT_MAX && path_copy_pays(left_len, right_len);
    let copy_right_path = right_len > FLAT_MAX && path_copy_pays(right_len, left_len);
    if !copy_left_path && !copy_right_path {
        let result = heap.merge_tree_bindings(roots, left, right)?;
        return Ok((result, heap.bindings_view(result).len()));
    }
    heap.with_roots(|heap, temporary| {
        let (result, written) = if copy_left_path {
            let root = tree_root(heap, temporary, left);
            let items = attributes(heap, temporary, right);
            let merged = insert_all(heap, temporary, root, &items, true)?;
            let written = merged.written;
            (from_merged(heap, temporary, merged, left_len)?, written)
        } else {
            let root = tree_root(heap, temporary, right);
            let items = attributes(heap, temporary, left);
            let merged = insert_all(heap, temporary, root, &items, false)?;
            let written = merged.written;
            (from_merged(heap, temporary, merged, right_len)?, written)
        };
        Ok((roots.copy(result), written))
    })
}

/// Updates from already sorted attributes without first allocating a set for
/// the small side. The empty update preserves the original set's identity.
pub(crate) fn update_sorted<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    items: &[RootedAttr<'_, 'h>],
) -> Result<BindingsRoot<'r, 'h>> {
    debug_assert!(items.windows(2).all(|pair| pair[0].name < pair[1].name));
    if items.is_empty() {
        return Ok(roots.copy(set));
    }
    let len = heap.bindings_view(set).len();
    heap.with_roots(|heap, temporary| {
        let result = if len > FLAT_MAX && path_copy_pays(len, items.len()) {
            let root = tree_root(heap, temporary, set);
            let merged = insert_all(heap, temporary, root, items, true)?;
            from_merged(heap, temporary, merged, len)?
        } else {
            let merged = merge_items(heap, temporary, set, items);
            build_sorted(heap, temporary, &merged, SrcPos::NONE)?
        };
        Ok(roots.copy(result))
    })
}

/// Always builds a fresh header without a source position, even if no name
/// exists in the set. Unchanged subtrees retain their identity.
pub(crate) fn remove<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    names: &[Symbol],
) -> Result<BindingsRoot<'r, 'h>> {
    let mut names = names.to_vec();
    names.sort_unstable();
    names.dedup();
    heap.with_roots(|heap, temporary| {
        let result = match heap.bindings_snapshot(temporary, set) {
            BindingsContents::Flat { attrs, .. } => {
                let kept = filter_out(&attrs, &names);
                heap.alloc_flat_bindings(temporary, SrcPos::NONE, &kept)?
            }
            BindingsContents::Tree { root, len, .. } => {
                let (replacement, removed) = remove_all(heap, temporary, root, &names)?;
                let len = len - removed;
                match replacement {
                    None => empty(heap, temporary, SrcPos::NONE)?,
                    Some(root) if len <= FLAT_MAX => {
                        let mut attrs = Vec::with_capacity(len);
                        collect_leaves(heap, temporary, root, &mut attrs);
                        heap.alloc_flat_bindings(temporary, SrcPos::NONE, &attrs)?
                    }
                    Some(root) => heap.alloc_tree_bindings(temporary, SrcPos::NONE, len, root)?,
                }
            }
        };
        Ok(roots.copy(result))
    })
}

/// Calls `f` in canonical order. Its input is rooted, so `f` may collect.
/// Names must be preserved, matching `mapAttrs` and the existing map contract.
pub(crate) fn map<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    pos: SrcPos,
    mut f: impl FnMut(&mut Heap<'h>, &Roots<'r, 'h>, RootedAttr<'r, 'h>) -> Result<RootedAttr<'r, 'h>>,
) -> Result<BindingsRoot<'r, 'h>> {
    let mut mapped = attributes(heap, roots, set);
    for attr in &mut mapped {
        let replacement = f(heap, roots, *attr)?;
        debug_assert_eq!(attr.name, replacement.name);
        *attr = replacement;
    }
    from_sorted(heap, roots, &mapped, pos)
}

/// Explicitly snapshots all attributes into the destination root frame.
/// Only full rebuilding/whole-set consumers use this; small updates do not
/// flatten the large operand.
pub(crate) fn attributes<'r, 'h>(
    heap: &Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
) -> Vec<RootedAttr<'r, 'h>> {
    let view = heap.bindings_view(set);
    let mut attrs = Vec::with_capacity(view.len());
    // No managed allocation occurs during this immutable heap borrow. Save
    // each cell directly instead of rooting every traversed internal node.
    attrs.extend(view.iter().map(|attr| attr.root(roots)));
    attrs
}

/// Names are sorted only for consumers that require lexical order; the tree
/// itself retains canonical symbol-id order.
pub(crate) fn lexicographic<'r, 'h>(
    heap: &Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    keys: &SymbolKeys<'_>,
) -> Vec<RootedAttr<'r, 'h>> {
    let mut keyed: Vec<_> = attributes(heap, roots, set)
        .into_iter()
        .map(|attr| (keys.prefix(attr.name), attr))
        .collect();
    keyed.sort_unstable_by(|(left_prefix, left), (right_prefix, right)| {
        left_prefix
            .cmp(right_prefix)
            .then_with(|| keys.name(left.name).cmp(keys.name(right.name)))
    });
    keyed.into_iter().map(|(_, attr)| attr).collect()
}

/// A names-only traversal never needs to root attribute values.
pub(crate) fn lexicographic_names<'h>(
    heap: &Heap<'h>,
    set: BindingsRoot<'_, 'h>,
    keys: &SymbolKeys<'_>,
) -> Vec<Symbol> {
    let view = heap.bindings_view(set);
    let mut names = Vec::with_capacity(view.len());
    names.extend(view.iter().map(|attr| attr.name));
    names.sort_unstable_by(|left, right| {
        keys.prefix(*left)
            .cmp(&keys.prefix(*right))
            .then_with(|| keys.name(*left).cmp(keys.name(*right)))
    });
    names
}

fn copy_attr<'r, 'h>(roots: &Roots<'r, 'h>, attr: RootedAttr<'_, 'h>) -> RootedAttr<'r, 'h> {
    RootedAttr {
        name: attr.name,
        pos: attr.pos,
        value: roots.copy(attr.value),
    }
}

fn tree_root<'r, 'h>(
    heap: &Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
) -> NodeRoot<'r, 'h> {
    match heap.bindings_snapshot(roots, set) {
        BindingsContents::Tree { root, .. } => root,
        BindingsContents::Flat { .. } => unreachable!("tree representation checked by length"),
    }
}

fn path_copy_pays(big: usize, small: usize) -> bool {
    // Equivalent to small * LEAF_MAX * 2 < big without multiplication overflow.
    small < big.div_ceil(LEAF_MAX * 2)
}

fn even_chunks(len: usize, max: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    let chunks = len.div_ceil(max).max(1);
    let base = len / chunks;
    let extra = len % chunks;
    let mut start = 0;
    (0..chunks).map(move |index| {
        let size = base + usize::from(index < extra);
        let range = start..start + size;
        start += size;
        range
    })
}

fn build_sorted<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    attrs: &[RootedAttr<'_, 'h>],
    pos: SrcPos,
) -> Result<BindingsRoot<'r, 'h>> {
    if attrs.len() <= FLAT_MAX {
        return heap.alloc_flat_bindings(roots, pos, attrs);
    }
    let mut nodes = Vec::with_capacity(attrs.len().div_ceil(LEAF_MAX));
    for range in even_chunks(attrs.len(), LEAF_MAX) {
        nodes.push(heap.alloc_leaf(roots, &attrs[range])?);
    }
    let root = build_above(heap, roots, nodes)?;
    heap.alloc_tree_bindings(roots, pos, attrs.len(), root)
}

fn build_above<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    mut nodes: Vec<NodeRoot<'r, 'h>>,
) -> Result<NodeRoot<'r, 'h>> {
    assert!(!nodes.is_empty(), "a tree has at least one leaf");
    while nodes.len() > 1 {
        let mut parents = Vec::with_capacity(nodes.len().div_ceil(INNER_MAX));
        for range in even_chunks(nodes.len(), INNER_MAX) {
            parents.push(heap.alloc_inner(roots, &nodes[range])?);
        }
        nodes = parents;
    }
    Ok(nodes[0])
}

fn collect_leaves<'r, 'h>(
    heap: &Heap<'h>,
    roots: &Roots<'r, 'h>,
    node: NodeRoot<'_, 'h>,
    out: &mut Vec<RootedAttr<'r, 'h>>,
) {
    match heap.node_snapshot(roots, node) {
        NodeContents::Leaf(attrs) => out.extend(attrs),
        NodeContents::Inner { children, .. } => {
            for child in children {
                collect_leaves(heap, roots, child, out);
            }
        }
    }
}

fn merge_items<'r, 'h>(
    heap: &Heap<'h>,
    roots: &Roots<'r, 'h>,
    set: BindingsRoot<'_, 'h>,
    items: &[RootedAttr<'_, 'h>],
) -> Vec<RootedAttr<'r, 'h>> {
    let old = heap.bindings_view(set);
    let mut merged = Vec::with_capacity(old.len() + items.len());
    let mut old = old.iter().peekable();
    let mut items = items.iter().peekable();
    while let (Some(a), Some(b)) = (old.peek(), items.peek()) {
        match a.name.cmp(&b.name) {
            Ordering::Less => merged.push(old.next().unwrap().root(roots)),
            Ordering::Greater => merged.push(copy_attr(roots, *items.next().unwrap())),
            Ordering::Equal => {
                old.next();
                merged.push(copy_attr(roots, *items.next().unwrap()));
            }
        }
    }
    merged.extend(old.map(|attr| attr.root(roots)));
    merged.extend(items.map(|attr| copy_attr(roots, *attr)));
    merged
}

fn merge_attrs<'r, 'h>(
    existing: &[RootedAttr<'r, 'h>],
    items: &[RootedAttr<'r, 'h>],
    replace: bool,
) -> Vec<RootedAttr<'r, 'h>> {
    let mut merged = Vec::with_capacity(existing.len() + items.len());
    let (mut i, mut j) = (0, 0);
    while i < existing.len() && j < items.len() {
        match existing[i].name.cmp(&items[j].name) {
            Ordering::Equal => {
                merged.push(if replace { items[j] } else { existing[i] });
                i += 1;
                j += 1;
            }
            Ordering::Less => {
                merged.push(existing[i]);
                i += 1;
            }
            Ordering::Greater => {
                merged.push(items[j]);
                j += 1;
            }
        }
    }
    merged.extend_from_slice(&existing[i..]);
    merged.extend_from_slice(&items[j..]);
    merged
}

struct Merged<'r, 'h> {
    nodes: Vec<NodeRoot<'r, 'h>>,
    added: usize,
    written: usize,
}

fn from_merged<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    merged: Merged<'r, 'h>,
    old_len: usize,
) -> Result<BindingsRoot<'r, 'h>> {
    let root = build_above(heap, roots, merged.nodes)?;
    heap.alloc_tree_bindings(roots, SrcPos::NONE, old_len + merged.added, root)
}

fn insert_all<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    node: NodeRoot<'r, 'h>,
    items: &[RootedAttr<'_, 'h>],
    replace: bool,
) -> Result<Merged<'r, 'h>> {
    match heap.node_snapshot(roots, node) {
        NodeContents::Leaf(attrs) => {
            let items: Vec<_> = items.iter().map(|attr| copy_attr(roots, *attr)).collect();
            let merged = merge_attrs(&attrs, &items, replace);
            let added = merged.len() - attrs.len();
            let written = merged.len();
            let mut nodes = Vec::with_capacity(merged.len().div_ceil(LEAF_MAX));
            for range in even_chunks(merged.len(), LEAF_MAX) {
                nodes.push(heap.alloc_leaf(roots, &merged[range])?);
            }
            Ok(Merged {
                nodes,
                added,
                written,
            })
        }
        NodeContents::Inner { children, firsts } => {
            let mut replacements = Vec::with_capacity(children.len() + 1);
            let (mut added, mut written, mut start) = (0, 0, 0);
            for (index, child) in children.into_iter().enumerate() {
                let end = match firsts.get(index + 1) {
                    Some(next) => start + items[start..].partition_point(|attr| attr.name < *next),
                    None => items.len(),
                };
                if start < end {
                    let merged = insert_all(heap, roots, child, &items[start..end], replace)?;
                    replacements.extend(merged.nodes);
                    added += merged.added;
                    written += merged.written;
                } else {
                    replacements.push(child);
                }
                start = end;
            }
            let mut nodes = Vec::with_capacity(replacements.len().div_ceil(INNER_MAX));
            for range in even_chunks(replacements.len(), INNER_MAX) {
                nodes.push(heap.alloc_inner(roots, &replacements[range])?);
            }
            Ok(Merged {
                nodes,
                added,
                written,
            })
        }
    }
}

fn filter_out<'r, 'h>(attrs: &[RootedAttr<'r, 'h>], names: &[Symbol]) -> Vec<RootedAttr<'r, 'h>> {
    let mut kept = Vec::with_capacity(attrs.len());
    let mut j = 0;
    for attr in attrs {
        while j < names.len() && names[j] < attr.name {
            j += 1;
        }
        if j < names.len() && names[j] == attr.name {
            j += 1;
        } else {
            kept.push(*attr);
        }
    }
    kept
}

fn remove_all<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    node: NodeRoot<'r, 'h>,
    names: &[Symbol],
) -> Result<(Option<NodeRoot<'r, 'h>>, usize)> {
    match heap.node_snapshot(roots, node) {
        NodeContents::Leaf(attrs) => {
            let kept = filter_out(&attrs, names);
            let removed = attrs.len() - kept.len();
            if removed == 0 {
                Ok((Some(node), 0))
            } else if kept.is_empty() {
                Ok((None, removed))
            } else {
                Ok((Some(heap.alloc_leaf(roots, &kept)?), removed))
            }
        }
        NodeContents::Inner { children, firsts } => {
            let mut replacements = Vec::with_capacity(children.len());
            let (mut removed, mut start) = (0, 0);
            for (index, child) in children.into_iter().enumerate() {
                let end = match firsts.get(index + 1) {
                    Some(next) => start + names[start..].partition_point(|name| name < next),
                    None => names.len(),
                };
                if start < end {
                    let (replacement, count) = remove_all(heap, roots, child, &names[start..end])?;
                    removed += count;
                    replacements.extend(replacement);
                } else {
                    replacements.push(child);
                }
                start = end;
            }
            if removed == 0 {
                Ok((Some(node), 0))
            } else if replacements.is_empty() {
                Ok((None, removed))
            } else if replacements.len() == 1 {
                Ok((Some(replacements[0]), removed))
            } else {
                Ok((Some(heap.alloc_inner(roots, &replacements)?), removed))
            }
        }
    }
}

#[cfg(all(test, not(brix_parallel)))]
mod tests {
    use super::*;
    use crate::managed_heap::{with_heap, CellRoot, ValueView};
    use brix_gc::Config;
    use std::collections::BTreeMap;

    fn config(stress: bool) -> Config {
        Config {
            workers: 0,
            chunk_bytes: 1024,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 8192,
            stress,
        }
    }

    fn attrs<'r, 'h>(
        cell: CellRoot<'r, 'h>,
        names: impl IntoIterator<Item = u32>,
    ) -> Vec<RootedAttr<'r, 'h>> {
        names
            .into_iter()
            .map(|name| RootedAttr {
                name: Symbol(name),
                pos: SrcPos(name + 1),
                value: cell,
            })
            .collect()
    }

    fn assert_model<'h>(
        heap: &Heap<'h>,
        roots: &Roots<'_, 'h>,
        set: BindingsRoot<'_, 'h>,
        model: &BTreeMap<u32, (u32, i64)>,
    ) {
        let view = heap.bindings_view(set);
        assert_eq!(view.len(), model.len());
        let mut iter = view.iter();
        for remaining in (1..=model.len()).rev() {
            assert_eq!(iter.len(), remaining);
            assert_eq!(iter.size_hint(), (remaining, Some(remaining)));
            assert!(iter.next().is_some());
        }
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
        let actual: Vec<_> = view
            .iter()
            .map(|attr| {
                let value = match heap.read(attr.value.root(roots)) {
                    ValueView::Int(value) => value,
                    _ => panic!("expected integer cell"),
                };
                (attr.name.0, (attr.pos.0, value))
            })
            .collect();
        assert_eq!(
            actual,
            model
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>()
        );
        for (&name, &(pos, _)) in model {
            assert_eq!(view.get(Symbol(name)).unwrap().pos.0, pos);
        }
        assert!(view.get(Symbol(u32::MAX)).is_none());
        match heap.bindings_snapshot(roots, set) {
            BindingsContents::Flat { attrs, .. } => assert!(attrs.len() <= FLAT_MAX),
            BindingsContents::Tree { root, len, .. } => {
                assert!(len > FLAT_MAX);
                let mut previous = None;
                assert_eq!(check_node(heap, roots, root, &mut previous).0, len);
            }
        }
    }

    fn check_node<'h>(
        heap: &Heap<'h>,
        roots: &Roots<'_, 'h>,
        node: NodeRoot<'_, 'h>,
        previous: &mut Option<Symbol>,
    ) -> (usize, Symbol) {
        match heap.node_snapshot(roots, node) {
            NodeContents::Leaf(attrs) => {
                assert!(!attrs.is_empty() && attrs.len() <= LEAF_MAX);
                for attr in &attrs {
                    assert!(previous.is_none_or(|name| name < attr.name));
                    *previous = Some(attr.name);
                }
                (attrs.len(), attrs[0].name)
            }
            NodeContents::Inner { children, firsts } => {
                assert!((2..=INNER_MAX).contains(&children.len()));
                assert_eq!(children.len(), firsts.len());
                let mut len = 0;
                for (child, first) in children.into_iter().zip(&firsts) {
                    let (count, actual_first) = check_node(heap, roots, child, previous);
                    assert_eq!(*first, actual_first);
                    len += count;
                }
                (len, firsts[0])
            }
        }
    }

    fn leaves<'r, 'h>(
        heap: &Heap<'h>,
        roots: &Roots<'r, 'h>,
        node: NodeRoot<'_, 'h>,
    ) -> Vec<NodeRoot<'r, 'h>> {
        match heap.node_snapshot(roots, node) {
            NodeContents::Leaf(_) => vec![roots.copy(node)],
            NodeContents::Inner { children, .. } => children
                .into_iter()
                .flat_map(|child| leaves(heap, roots, child))
                .collect(),
        }
    }

    #[test]
    fn flat_boundary_positions_sorting_and_empty_identity_survive_collection() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.int(42)).unwrap();
                for len in [0, 1, 8, 63, 64, 65, 127, 128, 129] {
                    heap.with_roots(|heap, temporary| {
                        let input = attrs(temporary.copy(cell), 0..len);
                        let set = from_sorted(heap, temporary, &input, SrcPos(99)).unwrap();
                        heap.collect().unwrap();
                        assert_eq!(heap.bindings_view(set).pos(), SrcPos(99));
                        assert_model(
                            heap,
                            temporary,
                            set,
                            &(0..len).map(|name| (name, (name + 1, 42))).collect(),
                        );
                        let nil = empty(heap, temporary, SrcPos(777)).unwrap();
                        let result = update(heap, temporary, set, nil).unwrap();
                        assert!(heap.same_bindings(if len == 0 { nil } else { set }, result));
                        let result = update(heap, temporary, nil, set).unwrap();
                        assert!(heap.same_bindings(set, result));
                        let result = update_sorted(heap, temporary, set, &[]).unwrap();
                        assert!(heap.same_bindings(set, result));
                        let removed = remove(heap, temporary, set, &[]).unwrap();
                        assert!(!heap.same_bindings(set, removed));
                        assert_eq!(heap.bindings_view(removed).pos(), SrcPos::NONE);
                    });
                }
                let mut duplicate = attrs(cell, [9, 2, 9, 1]);
                duplicate[2].pos = SrcPos(888);
                sort_dedup(&mut duplicate);
                assert_eq!(
                    duplicate.iter().map(|attr| attr.name.0).collect::<Vec<_>>(),
                    [1, 2, 9]
                );
                assert_eq!(duplicate[2].pos, SrcPos(888));
            })
        })
        .unwrap();
    }

    #[test]
    fn direct_flat_merges_reload_both_owners_and_preserve_right_positions() {
        with_heap(config(true), |heap| {
            for (left_names, right_names) in [
                (0..16, 8..24),
                (0..64, 0..64),
                (0..32, 32..64),
                (0..33, 33..66),
                (0..64, 1..65),
            ] {
                heap.with_roots(|heap, roots| {
                    let mut model = BTreeMap::new();
                    let (left, right) = heap.with_roots(|heap, temporary| {
                        let mut sides = Vec::new();
                        for (names, side) in [(left_names, 1), (right_names, 2)] {
                            let mut attributes = Vec::new();
                            for name in names {
                                let pos = side * 1000 + name;
                                let value = i64::from(side * 100 + name);
                                let cell = heap.cell(temporary, temporary.int(value)).unwrap();
                                attributes.push(RootedAttr {
                                    name: Symbol(name),
                                    pos: SrcPos(pos),
                                    value: cell,
                                });
                                model.insert(name, (pos, value));
                            }
                            let set =
                                from_sorted(heap, temporary, &attributes, SrcPos(side)).unwrap();
                            sides.push(roots.copy(set));
                        }
                        (sides[0], sides[1])
                    });
                    // Only the source bindings own their attribute cells here.
                    // Stress collection in reserve must update both owners.
                    let collections = heap.stats().collections;
                    let (result, written) = update_counted(heap, roots, left, right).unwrap();
                    if model.len() <= FLAT_MAX {
                        assert_eq!(heap.stats().collections, collections + 1);
                    }
                    assert_eq!(written, model.len());
                    assert_eq!(heap.bindings_view(result).pos(), SrcPos::NONE);
                    assert_eq!(heap.bindings_view(left).pos(), SrcPos(1));
                    assert_eq!(heap.bindings_view(right).pos(), SrcPos(2));
                    assert_model(heap, roots, result, &model);
                    heap.collect().unwrap();
                    assert_model(heap, roots, result, &model);
                });
            }
        })
        .unwrap();
    }

    #[test]
    fn small_updates_share_untouched_subtrees_and_copy_only_touched_leaves() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.int(42)).unwrap();
                let original =
                    from_sorted(heap, roots, &attrs(cell, 0..6000), SrcPos(123)).unwrap();
                let original_root = tree_root(heap, roots, original);
                let before = leaves(heap, roots, original_root);
                let mut patch = attrs(cell, [0, 6000]);
                patch[0].pos = SrcPos(999);
                let patch_set = from_sorted(heap, roots, &patch, SrcPos(456)).unwrap();
                let (merged, written) = update_counted(heap, roots, original, patch_set).unwrap();
                assert!(written <= LEAF_MAX * 3, "copied {written} attributes");
                assert_eq!(heap.bindings_view(merged).len(), 6001);
                assert_eq!(
                    heap.bindings_view(merged).get(Symbol(0)).unwrap().pos,
                    SrcPos(999)
                );
                assert_eq!(heap.bindings_view(merged).pos(), SrcPos::NONE);
                let after = leaves(heap, roots, tree_root(heap, roots, merged));
                let shared = before
                    .iter()
                    .filter(|&&old| after.iter().any(|&new| heap.same_node(old, new)))
                    .count();
                assert!(
                    shared >= before.len() - 2,
                    "shared {shared}/{} leaves",
                    before.len()
                );
                heap.collect().unwrap();
                assert_eq!(
                    heap.bindings_view(original).get(Symbol(0)).unwrap().pos,
                    SrcPos(1)
                );

                // When the tree is on the right, its existing attributes win.
                let (reverse, written) = update_counted(heap, roots, patch_set, original).unwrap();
                assert!(written <= LEAF_MAX * 3);
                assert_eq!(
                    heap.bindings_view(reverse).get(Symbol(0)).unwrap().pos,
                    SrcPos(1)
                );
                assert_eq!(
                    heap.bindings_view(reverse).get(Symbol(6000)).unwrap().pos,
                    SrcPos(6001)
                );
                let sorted = update_sorted(heap, roots, original, &patch).unwrap();
                assert_eq!(
                    heap.bindings_view(sorted).get(Symbol(0)).unwrap().pos,
                    SrcPos(999)
                );

                let unchanged = remove(heap, roots, original, &[Symbol(7000)]).unwrap();
                assert!(!heap.same_bindings(original, unchanged));
                assert!(heap.same_node(original_root, tree_root(heap, roots, unchanged)));
                let removed = remove(
                    heap,
                    roots,
                    original,
                    &(0..5990).map(Symbol).collect::<Vec<_>>(),
                )
                .unwrap();
                assert_eq!(heap.bindings_view(removed).len(), 10);
                assert_model(
                    heap,
                    roots,
                    removed,
                    &(5990..6000).map(|name| (name, (name + 1, 42))).collect(),
                );
            })
        })
        .unwrap();
    }

    #[test]
    fn full_merge_batch_collects_once_and_keeps_leaves_independently_reclaimable() {
        for stress in [false, true] {
            with_heap(config(stress), |heap| {
                let surviving_leaf = heap.with_roots(|heap, roots| {
                    let (left, right) = heap.with_roots(|heap, temporary| {
                        let left_cell = heap.cell(temporary, temporary.int(42)).unwrap();
                        let right_cell = heap.cell(temporary, temporary.int(84)).unwrap();
                        let left =
                            from_sorted(heap, temporary, &attrs(left_cell, 0..6000), SrcPos(1))
                                .unwrap();
                        let right =
                            from_sorted(heap, temporary, &attrs(right_cell, 3000..9000), SrcPos(2))
                                .unwrap();
                        (roots.copy(left), roots.copy(right))
                    });
                    let before = heap.stats().collections;
                    let (merged, written) = update_counted(heap, roots, left, right).unwrap();
                    if stress {
                        assert_eq!(heap.stats().collections, before + 1);
                    }
                    assert_eq!(written, 9000);
                    heap.collect().unwrap();
                    let model = (0..9000)
                        .map(|name| (name, (name + 1, if name < 3000 { 42 } else { 84 })))
                        .collect();
                    assert_model(heap, roots, merged, &model);
                    let root = tree_root(heap, roots, merged);
                    assert_eq!(check_node(heap, roots, root, &mut None).0, 9000);
                    let before = leaves(heap, roots, root);
                    // A subsequent minority update retains all unaffected leaves.
                    let cell = heap.cell(roots, roots.int(123)).unwrap();
                    let patch =
                        from_sorted(heap, roots, &attrs(cell, [0, 9000]), SrcPos::NONE).unwrap();
                    let updated = update(heap, roots, merged, patch).unwrap();
                    let after = leaves(heap, roots, tree_root(heap, roots, updated));
                    let shared = before
                        .iter()
                        .filter(|&&old| after.iter().any(|&new| heap.same_node(old, new)))
                        .count();
                    assert!(shared >= before.len() - 2);
                    heap.persist(before[0])
                });
                // No owner of the whole rebuilt tree remains. A single leaf
                // must not retain the other 8,936 attributes through a buffer.
                heap.collect().unwrap();
                assert!(
                    heap.used_bytes() < 4096,
                    "one leaf retained {} bytes",
                    heap.used_bytes()
                );
                heap.with_roots(|heap, roots| {
                    let NodeContents::Leaf(attrs) =
                        heap.node_snapshot(roots, surviving_leaf.root())
                    else {
                        panic!("leaf")
                    };
                    assert!(!attrs.is_empty() && attrs.len() <= LEAF_MAX);
                    for attr in attrs {
                        assert!(matches!(heap.read(attr.value), ValueView::Int(42)));
                    }
                });
            })
            .unwrap();
        }
    }

    #[test]
    fn full_merge_shares_whole_leaves_but_copies_consumed_fragments() {
        for stress in [false, true] {
            with_heap(config(stress), |heap| {
                heap.with_roots(|heap, roots| {
                    let left_cell = heap.cell(roots, roots.int(42)).unwrap();
                    let right_cell = heap.cell(roots, roots.int(84)).unwrap();
                    let left =
                        from_sorted(heap, roots, &attrs(left_cell, 0..128), SrcPos(1)).unwrap();
                    let right =
                        from_sorted(heap, roots, &attrs(right_cell, 128..256), SrcPos(2)).unwrap();
                    let original: Vec<_> = leaves(heap, roots, tree_root(heap, roots, left))
                        .into_iter()
                        .chain(leaves(heap, roots, tree_root(heap, roots, right)))
                        .collect();
                    let before = heap.space_usage()[crate::heap_layout::ATTRS].1;
                    let merged = update(heap, roots, left, right).unwrap();
                    assert_eq!(heap.space_usage()[crate::heap_layout::ATTRS].1, before);
                    let output = leaves(heap, roots, tree_root(heap, roots, merged));
                    assert_eq!(output.len(), original.len());
                    assert!(original
                        .iter()
                        .zip(&output)
                        .all(|(&a, &b)| heap.same_node(a, b)));
                    let identical = update(heap, roots, merged, merged).unwrap();
                    assert_eq!(heap.space_usage()[crate::heap_layout::ATTRS].1, before);
                    let again = leaves(heap, roots, tree_root(heap, roots, identical));
                    assert!(output
                        .iter()
                        .zip(&again)
                        .all(|(&a, &b)| heap.same_node(a, b)));

                    let patch =
                        from_sorted(heap, roots, &attrs(right_cell, [1]), SrcPos::NONE).unwrap();
                    let patched = update(heap, roots, left, patch).unwrap();
                    let patched_leaves = leaves(heap, roots, tree_root(heap, roots, patched));
                    assert!(!patched_leaves
                        .iter()
                        .any(|&node| heap.same_node(node, original[0])));
                    assert!(patched_leaves
                        .iter()
                        .any(|&node| heap.same_node(node, original[1])));
                    heap.collect().unwrap();
                    assert_model(
                        heap,
                        roots,
                        patched,
                        &(0..128)
                            .map(|name| (name, (name + 1, if name == 1 { 84 } else { 42 })))
                            .collect(),
                    );
                })
            })
            .unwrap();
        }
    }

    #[test]
    fn shared_merge_partial_flush_failures_preserve_source_graphs() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.int(42)).unwrap();
                let left = from_sorted(heap, roots, &attrs(cell, 0..128), SrcPos::NONE).unwrap();
                let right = from_sorted(
                    heap,
                    roots,
                    &attrs(cell, std::iter::once(0).chain(129..193)),
                    SrcPos::NONE,
                )
                .unwrap();
                heap.collect().unwrap();
                let baseline = heap.used_bytes();
                let mut completed = false;
                for failure in 0..32 {
                    let succeeded = roots.with_roots(|temporary| {
                        match crate::managed_heap::with_tree_merge_failure(failure, || {
                            update(heap, temporary, left, right)
                        }) {
                            Err(brix_gc::Error::OutOfMemory) => false,
                            Err(error) => panic!("unexpected batch error: {error:?}"),
                            Ok(merged) => {
                                assert_model(
                                    heap,
                                    temporary,
                                    merged,
                                    &(0..128)
                                        .chain(129..193)
                                        .map(|name| (name, (name + 1, 42)))
                                        .collect(),
                                );
                                true
                            }
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(heap.used_bytes(), baseline);
                    if succeeded {
                        completed = true;
                        break;
                    }
                }
                assert!(completed, "sweep must reach a successful complete batch");
                assert_model(
                    heap,
                    roots,
                    left,
                    &(0..128).map(|name| (name, (name + 1, 42))).collect(),
                );
                assert_model(
                    heap,
                    roots,
                    right,
                    &std::iter::once(0)
                        .chain(129..193)
                        .map(|name| (name, (name + 1, 42)))
                        .collect(),
                );
            })
        })
        .unwrap();
    }

    #[test]
    fn full_merge_leaf_cursors_handle_boundary_duplicates_and_disjoint_tails() {
        let cases: [(Vec<u32>, Vec<u32>); 7] = [
            ((0..129).collect(), vec![42, 43, 85, 86]),
            ((0..258).step_by(2).collect(), (1..258).step_by(2).collect()),
            ((0..129).collect(), (1000..1129).collect()),
            ((1000..1129).collect(), (0..129).collect()),
            ((0..65).collect(), (0..4097).collect()),
            ((0..4097).collect(), (4032..4097).collect()),
            ((0..65).collect(), (0..65).collect()),
        ];
        for stress in [false, true] {
            with_heap(config(stress), |heap| {
                for (left_names, right_names) in &cases {
                    heap.with_roots(|heap, roots| {
                        let mut model = BTreeMap::new();
                        let (left, right) = heap.with_roots(|heap, temporary| {
                            let mut sets = Vec::new();
                            for (names, side) in [(left_names, 1), (right_names, 2)] {
                                let cell = heap
                                    .cell(temporary, temporary.int(i64::from(side * 42)))
                                    .unwrap();
                                let mut input = attrs(cell, names.iter().copied());
                                for attr in &mut input {
                                    attr.pos = SrcPos(side * 100_000 + attr.name.0);
                                    model.insert(attr.name.0, (attr.pos.0, i64::from(side * 42)));
                                }
                                let set =
                                    from_sorted(heap, temporary, &input, SrcPos(side)).unwrap();
                                sets.push(roots.copy(set));
                            }
                            (sets[0], sets[1])
                        });
                        let before = heap.stats().collections;
                        let (merged, written) = update_counted(heap, roots, left, right).unwrap();
                        if stress {
                            assert_eq!(heap.stats().collections, before + 1);
                        }
                        assert_eq!(written, model.len());
                        assert_model(heap, roots, merged, &model);
                        heap.collect().unwrap();
                        assert_model(heap, roots, merged, &model);
                        assert_eq!(
                            check_node(heap, roots, tree_root(heap, roots, merged), &mut None).0,
                            model.len()
                        );
                    });
                }
            })
            .unwrap();
        }
    }

    #[test]
    fn full_merge_preflight_includes_pending_output_storage() {
        let threshold = 48 * 1024;
        with_heap(
            Config {
                workers: 0,
                chunk_bytes: 1024,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: threshold,
                stress: false,
            },
            |heap| {
                heap.with_roots(|heap, roots| {
                    let cell = heap.cell(roots, roots.int(42)).unwrap();
                    // Interleaved ranges require copied leaves; disjoint
                    // ranges now share leaves and can remain below threshold.
                    let left = from_sorted(
                        heap,
                        roots,
                        &attrs(cell, (0..1000).map(|i| i * 2)),
                        SrcPos::NONE,
                    )
                    .unwrap();
                    let right = from_sorted(
                        heap,
                        roots,
                        &attrs(cell, (0..1000).map(|i| i * 2 + 1)),
                        SrcPos::NONE,
                    )
                    .unwrap();
                    assert_eq!(heap.stats().collections, 0);
                    assert!(heap.used_bytes() < threshold);
                    let merged = update(heap, roots, left, right).unwrap();
                    assert_eq!(heap.stats().collections, 1);
                    assert!(heap.used_bytes() > threshold);
                    assert_model(
                        heap,
                        roots,
                        merged,
                        &(0..2000).map(|name| (name, (name + 1, 42))).collect(),
                    );
                });
            },
        )
        .unwrap();
    }

    #[test]
    fn partial_tree_merge_allocation_failures_leave_sources_and_heap_valid() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let (left, right) = heap.with_roots(|heap, temporary| {
                    let cell = heap.cell(temporary, temporary.int(42)).unwrap();
                    let left =
                        from_sorted(heap, temporary, &attrs(cell, 0..128), SrcPos(1)).unwrap();
                    let right =
                        from_sorted(heap, temporary, &attrs(cell, 64..192), SrcPos(2)).unwrap();
                    (roots.copy(left), roots.copy(right))
                });
                heap.collect().unwrap();
                let baseline = heap.used_bytes();
                // Two disjoint leaves are shared. One overlapping leaf array
                // and node + three inner payloads + one header are allocated.
                for failure in 0..=6 {
                    heap.with_roots(|heap, temporary| {
                        let result = crate::managed_heap::with_tree_merge_failure(failure, || {
                            update(heap, temporary, left, right)
                        });
                        if failure < 6 {
                            assert!(matches!(result, Err(brix_gc::Error::OutOfMemory)));
                        } else {
                            let merged = result.unwrap();
                            assert_model(
                                heap,
                                temporary,
                                merged,
                                &(0..192).map(|name| (name, (name + 1, 42))).collect(),
                            );
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(heap.used_bytes(), baseline);
                    assert_model(
                        heap,
                        roots,
                        left,
                        &(0..128).map(|name| (name, (name + 1, 42))).collect(),
                    );
                    assert_model(
                        heap,
                        roots,
                        right,
                        &(64..192).map(|name| (name, (name + 1, 42))).collect(),
                    );
                }
            });
        })
        .unwrap();
    }

    #[test]
    fn path_copying_splits_full_inner_nodes_and_removal_collapses_them() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.int(42)).unwrap();
                // Exactly 64 full leaves under one full inner node. Both new
                // names split a leaf, forcing that inner node to split too.
                let original = from_sorted(
                    heap,
                    roots,
                    &attrs(cell, (0..4096).map(|name| name * 2)),
                    SrcPos::NONE,
                )
                .unwrap();
                let patch =
                    from_sorted(heap, roots, &attrs(cell, [1, 8193]), SrcPos::NONE).unwrap();
                let (merged, written) = update_counted(heap, roots, original, patch).unwrap();
                assert_eq!(written, 2 * (LEAF_MAX + 1));
                let mut model: BTreeMap<_, _> = (0..4096)
                    .map(|name| name * 2)
                    .chain([1, 8193])
                    .map(|name| (name, (name + 1, 42)))
                    .collect();
                assert_model(heap, roots, merged, &model);
                let old_root = tree_root(heap, roots, original);
                let new_root = tree_root(heap, roots, merged);
                let NodeContents::Inner {
                    children: old_children,
                    ..
                } = heap.node_snapshot(roots, old_root)
                else {
                    panic!("old inner")
                };
                let NodeContents::Inner {
                    children: new_children,
                    ..
                } = heap.node_snapshot(roots, new_root)
                else {
                    panic!("new inner")
                };
                assert_eq!(old_children.len(), INNER_MAX);
                assert_eq!(new_children.len(), 2);
                assert!(new_children.iter().all(|&node| matches!(
                    heap.node_snapshot(roots, node),
                    NodeContents::Inner { .. }
                )));

                let names: Vec<_> = model
                    .keys()
                    .take(model.len() - 65)
                    .copied()
                    .map(Symbol)
                    .collect();
                for name in &names {
                    model.remove(&name.0);
                }
                let reduced = remove(heap, roots, merged, &names).unwrap();
                heap.collect().unwrap();
                assert_model(heap, roots, reduced, &model);
                let names: Vec<_> = model.keys().copied().map(Symbol).collect();
                let nil = remove(heap, roots, reduced, &names).unwrap();
                assert!(heap.bindings_view(nil).is_empty());
            })
        })
        .unwrap();
    }

    #[test]
    fn mapping_can_collect_while_previous_attributes_and_results_remain_rooted() {
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.int(42)).unwrap();
                let set = from_sorted(heap, roots, &attrs(cell, 0..180), SrcPos(1)).unwrap();
                let mapped = map(heap, roots, set, SrcPos(77), |heap, roots, attr| {
                    let old = match heap.read(attr.value) {
                        ValueView::Int(value) => value,
                        _ => unreachable!(),
                    };
                    let value = heap.cell(roots, roots.int(old + i64::from(attr.name.0)))?;
                    heap.collect()?;
                    Ok(RootedAttr {
                        value,
                        pos: SrcPos(attr.pos.0 + 1000),
                        ..attr
                    })
                })
                .unwrap();
                heap.collect().unwrap();
                assert_eq!(heap.bindings_view(mapped).pos(), SrcPos(77));
                assert_model(
                    heap,
                    roots,
                    mapped,
                    &(0..180)
                        .map(|name| (name, (name + 1001, 42 + i64::from(name))))
                        .collect(),
                );
                assert_model(
                    heap,
                    roots,
                    set,
                    &(0..180).map(|name| (name, (name + 1, 42))).collect(),
                );
            })
        })
        .unwrap();
    }

    #[test]
    fn collecting_from_fn_and_lexical_order_preserve_symbol_order() {
        use crate::symbol::{SymbolKeys, SymbolTable};
        let arena = bumpalo::Bump::new();
        let keys = SymbolKeys::new(&arena);
        let mut table = SymbolTable::new(keys);
        let names: Vec<_> = [
            b"prefix__z".as_slice(),
            b"prefix__a",
            b"prefix__m",
            b"prefix__b",
        ]
        .into_iter()
        .map(|name| table.intern(name))
        .collect();
        with_heap(config(true), |heap| {
            heap.with_roots(|heap, roots| {
                let set = from_fn(heap, roots, names.len(), SrcPos(5), |heap, roots, index| {
                    let value = heap.cell(roots, roots.int(index as i64))?;
                    Ok(RootedAttr {
                        name: names[index],
                        pos: SrcPos(index as u32),
                        value,
                    })
                })
                .unwrap();
                heap.collect().unwrap();
                assert_eq!(
                    heap.bindings_view(set)
                        .iter()
                        .map(|attr| attr.name)
                        .collect::<Vec<_>>(),
                    names
                );
                let sorted = lexicographic(heap, roots, set, keys);
                assert_eq!(
                    lexicographic_names(heap, set, keys),
                    [names[1], names[3], names[2], names[0]]
                );
                assert_eq!(
                    sorted.iter().map(|attr| attr.name).collect::<Vec<_>>(),
                    [names[1], names[3], names[2], names[0]]
                );
                let model = names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| (name.0, (index as u32, index as i64)))
                    .collect();
                assert_model(heap, roots, set, &model);
            })
        })
        .unwrap();
    }

    #[test]
    fn randomized_updates_removals_and_mapping_match_an_owned_model() {
        use rand::{RngExt, SeedableRng};
        use rand_pcg::Pcg64;
        for stress in [false, true] {
            with_heap(config(stress), |heap| {
                heap.with_roots(|heap, roots| {
                    let mut rng = Pcg64::seed_from_u64(0xb1d1_0650);
                    let cell = heap.cell(roots, roots.int(42)).unwrap();
                    let start = empty(heap, roots, SrcPos::NONE).unwrap();
                    let current = roots.slot(start);
                    let mut model = BTreeMap::new();
                    let mut version = 0;
                    for step in 0..90 {
                        heap.with_roots(|heap, temporary| {
                            let next = match step % 5 {
                                0 | 1 => {
                                    let count = if step % 5 == 0 { 180 } else { 3 };
                                    let mut patch = Vec::new();
                                    let mut changes = BTreeMap::new();
                                    for _ in 0..count {
                                        let name = rng.random_range(0..1100);
                                        version += 1;
                                        patch.push(RootedAttr {
                                            name: Symbol(name),
                                            pos: SrcPos(version),
                                            value: temporary.copy(cell),
                                        });
                                        changes.insert(name, (version, 42));
                                    }
                                    sort_dedup(&mut patch);
                                    if step % 3 == 0 {
                                        for (name, value) in changes {
                                            model.entry(name).or_insert(value);
                                        }
                                        let patch =
                                            from_sorted(heap, temporary, &patch, SrcPos::NONE)
                                                .unwrap();
                                        update(heap, temporary, patch, current.root()).unwrap()
                                    } else {
                                        model.extend(changes);
                                        update_sorted(heap, temporary, current.root(), &patch)
                                            .unwrap()
                                    }
                                }
                                2 => {
                                    let names: Vec<_> = (0..80)
                                        .map(|_| Symbol(rng.random_range(0..1100)))
                                        .collect();
                                    for name in &names {
                                        model.remove(&name.0);
                                    }
                                    remove(heap, temporary, current.root(), &names).unwrap()
                                }
                                _ => {
                                    for (position, value) in model.values_mut() {
                                        *position += 1;
                                        *value += 1;
                                    }
                                    map(
                                        heap,
                                        temporary,
                                        current.root(),
                                        SrcPos::NONE,
                                        |heap, roots, attr| {
                                            let old = match heap.read(attr.value) {
                                                ValueView::Int(value) => value,
                                                _ => unreachable!(),
                                            };
                                            let value = heap.cell(roots, roots.int(old + 1))?;
                                            Ok(RootedAttr {
                                                value,
                                                pos: SrcPos(attr.pos.0 + 1),
                                                ..attr
                                            })
                                        },
                                    )
                                    .unwrap()
                                }
                            };
                            current.set(next);
                            assert_model(heap, temporary, current.root(), &model);
                        });
                        heap.collect().unwrap();
                        heap.with_roots(|heap, temporary| {
                            assert_model(heap, temporary, current.root(), &model)
                        });
                    }
                })
            })
            .unwrap();
        }
    }
}
