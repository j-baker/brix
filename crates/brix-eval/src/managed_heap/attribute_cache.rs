//! Bounded lookup memoisation for immutable published attribute sets.
//!
//! Entries do not retain their sets or cells. GC epochs invalidate every entry
//! before an address can be moved or reused, and returned views borrow the heap.
use super::*;

const SLOTS: usize = 4096;

#[derive(Clone, Copy)]
struct Entry {
    address: usize,
    value: Option<CellPtr>,
    name: Symbol,
    pos: SrcPos,
}

impl Entry {
    const EMPTY: Self = Self {
        address: 0,
        value: None,
        name: Symbol(0),
        pos: SrcPos::NONE,
    };

    fn view<'v, 'h>(self) -> Option<AttrView<'v, 'h>> {
        self.value.map(|pointer| AttrView {
            name: self.name,
            pos: self.pos,
            value: CellView::new(pointer),
        })
    }
}

#[derive(Default)]
pub(super) struct AttributeCache {
    collection: usize,
    entries: Vec<Entry>,
}

#[inline]
fn slot(address: usize, name: Symbol, mask: usize) -> usize {
    let mixed =
        (address >> 3).wrapping_mul(0x9e3779b9) ^ (name.0 as usize).wrapping_mul(0x85ebca6b);
    (mixed ^ (mixed >> (usize::BITS / 2))) & mask
}

impl<'h> Heap<'h> {
    /// Memoise an attribute lookup, including absent names, without forcing the
    /// cell. Unlike classification caches, this also preserves source position.
    #[inline]
    pub(crate) fn lookup_attribute<'v>(
        &'v self,
        set: BindingsView<'v, 'h>,
        name: Symbol,
    ) -> Option<AttrView<'v, 'h>> {
        let address = set.pointer.as_ptr() as usize;
        let index = slot(address, name, SLOTS - 1);
        {
            let cache = self.attribute_cache.borrow();
            if cache.collection == self.raw.stats().collections && !cache.entries.is_empty() {
                let entry = cache.entries[index];
                if entry.address == address && entry.name == name {
                    // The set is live in this epoch; it still owns this cell.
                    // The heap borrow prevents GC for the returned view's life.
                    return entry.view();
                }
            }
        }
        self.lookup_attribute_miss(set, name, address, index)
    }

    #[inline(never)]
    fn lookup_attribute_miss<'v>(
        &'v self,
        set: BindingsView<'v, 'h>,
        name: Symbol,
        address: usize,
        index: usize,
    ) -> Option<AttrView<'v, 'h>> {
        let attr = set.get(name);
        // Neither lookup nor native cache allocation can collect the managed
        // heap, so address and cell remain current until the entry is installed.
        let mut cache = self.attribute_cache.borrow_mut();
        if cache.entries.is_empty() {
            cache.entries = vec![Entry::EMPTY; SLOTS];
        }
        let collection = self.raw.stats().collections;
        if cache.collection != collection {
            cache.entries.fill(Entry::EMPTY);
            cache.collection = collection;
        }
        cache.entries[index] = Entry {
            address,
            name,
            value: attr.map(|attr| attr.value.pointer),
            pos: attr.map_or(SrcPos::NONE, |attr| attr.pos),
        };
        attr
    }
}

#[cfg(feature = "work-counters")]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ProbeKey {
    address: usize,
    name: u32,
}

#[cfg(feature = "work-counters")]
#[derive(Default)]
pub(super) struct AttributeProbeReuse {
    collection: usize,
    tables: [Vec<ProbeKey>; 2],
}

#[cfg(feature = "work-counters")]
impl<'h> Heap<'h> {
    /// Observe lookups without changing evaluation or bypassing the real lookup.
    /// Uses the production hash and replacement policy at two bounded sizes.
    pub(crate) fn profile_attribute_lookup(
        &self,
        stats: &crate::stats::Stats,
        set: BindingsView<'_, 'h>,
        name: Symbol,
        found: bool,
        site: usize,
    ) {
        crate::stats::Stats::bump_work(&stats.attribute_probe_total);
        crate::stats::Stats::bump_work(&stats.attribute_probe_sites[site][0]);
        if !found {
            crate::stats::Stats::bump_work(&stats.attribute_probe_missing);
        }
        crate::stats::Stats::bump_work(
            &stats.attribute_probe_shapes[usize::from(set.len() > crate::bindings::FLAT_MAX)],
        );
        let key = ProbeKey {
            address: set.pointer.as_ptr() as usize,
            name: name.0,
        };
        let mut reuse = self.attribute_probe_reuse.borrow_mut();
        if reuse.tables[0].is_empty() {
            reuse.tables = [
                vec![ProbeKey::default(); 4096],
                vec![ProbeKey::default(); 16384],
            ];
        }
        let collection = self.raw.stats().collections;
        if reuse.collection != collection {
            reuse.collection = collection;
            for table in &mut reuse.tables {
                table.fill(ProbeKey::default());
            }
        }
        for (index, table) in reuse.tables.iter_mut().enumerate() {
            let at = slot(key.address, name, table.len() - 1);
            if table[at] == key {
                crate::stats::Stats::bump_work(&stats.attribute_probe_reuse[index]);
                crate::stats::Stats::bump_work(&stats.attribute_probe_sites[site][index + 1]);
            }
            table[at] = key;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_attributes_preserve_cells_positions_missing_names_and_gc_epochs() {
        for workers in [1, 4] {
            with_owned_heap(
                Config {
                    workers,
                    ..Config::default()
                },
                |mut heap| {
                    heap.with_roots(|heap, roots| {
                        let present = Symbol(17);
                        let missing = Symbol(18);
                        let set = heap.with_roots(|heap, temp| {
                            let value = heap.cell(temp, temp.int(41)).unwrap();
                            let set = heap
                                .alloc_flat_bindings(
                                    temp,
                                    SrcPos(12),
                                    &[RootedAttr {
                                        name: present,
                                        pos: SrcPos(23),
                                        value,
                                    }],
                                )
                                .unwrap();
                            roots.copy(set)
                        });
                        for major in [false, false, true] {
                            for _ in 0..2 {
                                let attr = heap
                                    .lookup_attribute(heap.bindings_view(set), present)
                                    .unwrap();
                                assert_eq!(attr.name, present);
                                assert_eq!(attr.pos, SrcPos(23));
                                assert!(matches!(heap.read_view(attr.value), ValueView::Int(41)));
                                assert!(heap
                                    .lookup_attribute(heap.bindings_view(set), missing)
                                    .is_none());
                            }
                            let cell = heap
                                .lookup_attribute(heap.bindings_view(set), present)
                                .unwrap()
                                .value
                                .root(roots);
                            heap.store(cell, roots.int(42));
                            let attr = heap
                                .lookup_attribute(heap.bindings_view(set), present)
                                .unwrap();
                            assert!(matches!(heap.read_view(attr.value), ValueView::Int(42)));
                            if major {
                                heap.collect().unwrap();
                            } else {
                                heap.collect_minor().unwrap();
                            }
                            assert_ne!(
                                heap.attribute_cache.borrow().collection,
                                heap.stats().collections
                            );
                            let attr = heap
                                .lookup_attribute(heap.bindings_view(set), present)
                                .unwrap();
                            assert_eq!(attr.pos, SrcPos(23));
                            assert!(heap.same_cell(attr.value.root(roots), cell));
                            assert!(matches!(heap.read_view(attr.value), ValueView::Int(42)));
                            heap.store(cell, roots.int(41));
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(
                        heap.used_bytes(),
                        0,
                        "lookup cache must not retain sets or cells"
                    );
                },
            )
            .unwrap();
        }
    }

    #[cfg(feature = "work-counters")]
    #[test]
    fn probe_reuse_counts_missing_names_but_not_prior_gc_epochs() {
        with_owned_heap(Config::default(), |mut heap| {
            heap.with_roots(|heap, roots| {
                let set = heap.alloc_flat_bindings(roots, SrcPos(17), &[]).unwrap();
                let stats = crate::stats::Stats::default();
                for name in [Symbol(17), Symbol(18), Symbol(17), Symbol(18)] {
                    heap.profile_attribute_lookup(&stats, heap.bindings_view(set), name, false, 0);
                }
                heap.collect().unwrap();
                heap.profile_attribute_lookup(
                    &stats,
                    heap.bindings_view(set),
                    Symbol(17),
                    false,
                    0,
                );
                assert_eq!(stats.attribute_probe_total.get(), 5);
                assert_eq!(stats.attribute_probe_missing.get(), 5);
                assert_eq!(stats.attribute_probe_shapes[0].get(), 5);
                for hits in &stats.attribute_probe_reuse {
                    assert_eq!(hits.get(), 2);
                }
            });
        })
        .unwrap();
    }
}
