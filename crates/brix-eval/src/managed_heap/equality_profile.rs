//! Optional, bounded probe-reuse measurements. Keys are observations only:
//! they are never dereferenced or used to bypass evaluation, and are cleared
//! after every collection so moved/reused addresses cannot count as hits.
use super::*;

#[derive(Default)]
pub(super) struct ProbeReuse {
    collection: usize,
    tables: [Vec<usize>; 2],
}

impl<'h> Heap<'h> {
    pub(crate) fn profile_derivation_probe(
        &self,
        stats: &crate::stats::Stats,
        set: BindingsRoot<'_, 'h>,
        attr: Option<CellView<'_, 'h>>,
    ) {
        let category = match attr.map(|attr| self.read_view(attr)) {
            None => 0,
            Some(ValueView::String(s)) => {
                if s.contexts()
                    .any(|c| matches!(c, ContextView::LazyHash { .. }))
                {
                    4
                } else if s.bytes() == b"derivation" {
                    2
                } else {
                    3
                }
            }
            Some(ValueView::Thunk { .. } | ValueView::App(_)) => 5,
            Some(ValueView::Blackhole | ValueView::Failed(_)) => 6,
            Some(_) => 1,
        };
        crate::stats::Stats::bump_work(&stats.derivation_probe_types[category]);
        let len = self.bindings_view(set).len();
        stats.derivation_probe_sizes.record(len);
        crate::stats::Stats::bump_work(
            &stats.derivation_probe_shapes[usize::from(len > crate::bindings::FLAT_MAX)],
        );
        let key = self.bindings_identity(set).address;
        let mut reuse = self.probe_reuse.borrow_mut();
        if reuse.tables[0].is_empty() {
            reuse.tables = [vec![0; 4096], vec![0; 65536]];
        }
        let collection = self.raw.stats().collections;
        if reuse.collection != collection {
            reuse.collection = collection;
            for table in &mut reuse.tables {
                table.fill(0);
            }
        }
        // Mix high address bits too; objects are aligned and slabs are clustered.
        let mixed = (key >> 3).wrapping_mul(0x9e3779b9);
        let mixed = mixed ^ (mixed >> (usize::BITS / 2));
        for (index, table) in reuse.tables.iter_mut().enumerate() {
            let slot = mixed & (table.len() - 1);
            if table[slot] == key {
                crate::stats::Stats::bump_work(&stats.derivation_probe_reuse[index]);
            }
            table[slot] = key;
        }
    }
}
