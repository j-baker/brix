//! Optional, bounded probe-reuse measurements. Keys are observations only:
//! they are never dereferenced or used to bypass evaluation, and the epoch
//! gate clears them after every collection so moved/reused addresses cannot
//! count as hits.
use super::identity::{AddressTag, EpochGated};
use super::*;

#[derive(Default)]
pub(super) struct ProbeReuse {
    gate: EpochGated<[Vec<AddressTag>; 2]>,
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
        let tag = AddressTag::new(self.bindings_identity(set));
        let mut reuse = self.probe_reuse.borrow_mut();
        let tables = reuse.gate.write_current(self.epoch(), |tables| {
            for table in tables {
                table.fill(AddressTag::EMPTY);
            }
        });
        if tables[0].is_empty() {
            *tables = [
                vec![AddressTag::EMPTY; 4096],
                vec![AddressTag::EMPTY; 65536],
            ];
        }
        for (index, table) in tables.iter_mut().enumerate() {
            let slot = tag.slot(table.len() - 1);
            if table[slot] == tag {
                crate::stats::Stats::bump_work(&stats.derivation_probe_reuse[index]);
            }
            table[slot] = tag;
        }
    }
}
