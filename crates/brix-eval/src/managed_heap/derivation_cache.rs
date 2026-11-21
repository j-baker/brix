//! Bounded memoisation of completed derivation classifications. Entries are
//! opaque address tags and untraced outPath cells. The epoch gate invalidates
//! every entry before either keys or cells can be reused.
use super::identity::{AddressTag, EpochGated};
use super::*;

const SLOTS: usize = 4096;

#[derive(Clone, Copy, Default)]
struct Entry {
    tag: AddressTag,
    out_path: Option<CellPtr>,
}

#[derive(Default)]
pub(super) struct DerivationCache {
    gate: EpochGated<Vec<Entry>>,
}

impl<'h> Heap<'h> {
    /// Read both classification and output in one probe. Only completed
    /// positive entries with an output can take the borrowed comparison path.
    /// The set keeps its cell alive; this heap borrow excludes GC until the
    /// returned view has been consumed. No tag survives an epoch change.
    #[inline]
    pub(crate) fn cached_derivation_output<'v>(
        &'v self,
        set: BindingsView<'v, 'h>,
    ) -> Option<CellView<'v, 'h>> {
        let cache = self.derivation_cache.borrow();
        let entries = cache.gate.read(self.epoch())?;
        if entries.is_empty() {
            return None;
        }
        let identity = self.view_identity(set);
        let entry = entries[AddressTag::new(identity).slot(SLOTS - 1)];
        if entry.tag.matches_flagged(identity, true) {
            entry.out_path.map(CellView::new)
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn cached_derivation(&self, set: BindingsRoot<'_, 'h>) -> Option<bool> {
        let cache = self.derivation_cache.borrow();
        let entries = cache.gate.read(self.epoch())?;
        if entries.is_empty() {
            return None;
        }
        let identity = self.bindings_identity(set);
        let entry = entries[AddressTag::new(identity).slot(SLOTS - 1)];
        entry
            .tag
            .matches_any_flag(identity)
            .then_some(entry.tag.flag())
    }

    /// Published bindings are immutable and keep their cells alive. The epoch
    /// gate excludes moved/freed pointers, and the returned view borrows the
    /// heap so it cannot survive collection. Cache misses use the rooted set.
    #[inline]
    pub(crate) fn derivation_out_path<'v>(
        &'v self,
        set: BindingsRoot<'_, 'h>,
    ) -> Option<CellView<'v, 'h>> {
        let cache = self.derivation_cache.borrow();
        if let Some(entries) = cache.gate.read(self.epoch()) {
            if !entries.is_empty() {
                let identity = self.bindings_identity(set);
                let entry = entries[AddressTag::new(identity).slot(SLOTS - 1)];
                if entry.tag.matches_flagged(identity, true) {
                    return entry.out_path.map(CellView::new);
                }
            }
        }
        self.bindings_view(set)
            .get(crate::symbol::sym::OUT_PATH)
            .map(|attr| attr.value)
    }

    /// Only call after classification succeeds with a stable type value (or
    /// no type attribute). Never cache failures, suspensions or lazy hashes.
    pub(crate) fn cache_derivation(&self, set: BindingsRoot<'_, 'h>, value: bool) {
        let tag = AddressTag::new(self.bindings_identity(set));
        // Looking up an attribute does not force its cell or collect.
        let out_path = if value {
            self.bindings_view(set)
                .get(crate::symbol::sym::OUT_PATH)
                .map(|attr| attr.value.pointer)
        } else {
            None
        };
        let mut cache = self.derivation_cache.borrow_mut();
        let entries = cache
            .gate
            .write_current(self.epoch(), |entries| entries.fill(Entry::default()));
        if entries.is_empty() {
            *entries = vec![Entry::default(); SLOTS];
        }
        entries[tag.slot(SLOTS - 1)] = Entry {
            tag: tag.with_flag(value),
            out_path,
        };
    }
}
