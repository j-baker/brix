//! Bounded memoisation of completed derivation classifications. Entries are
//! address tags and untraced outPath cells. Every successful GC changes the
//! epoch, invalidating entries before either keys or cells can be reused.
use super::*;

const SLOTS: usize = 4096;

#[derive(Clone, Copy, Default)]
struct Entry {
    tag: usize,
    out_path: Option<CellPtr>,
}

#[derive(Default)]
pub(super) struct DerivationCache {
    collection: usize,
    entries: Vec<Entry>,
}

#[inline]
fn slot(key: usize) -> usize {
    let mixed = (key >> 3).wrapping_mul(0x9e3779b9);
    (mixed ^ (mixed >> (usize::BITS / 2))) & (SLOTS - 1)
}

impl<'h> Heap<'h> {
    /// Read both classification and output in one probe. Only completed
    /// positive entries with an output can take the borrowed comparison path.
    /// The set keeps its cell alive; this heap borrow excludes GC until the
    /// returned view has been consumed. No address survives an epoch change.
    #[inline]
    pub(crate) fn cached_derivation_output<'v>(
        &'v self,
        set: BindingsView<'v, 'h>,
    ) -> Option<CellView<'v, 'h>> {
        let cache = self.derivation_cache.borrow();
        if cache.collection != self.raw.stats().collections || cache.entries.is_empty() {
            return None;
        }
        let key = set.pointer.as_ptr() as usize;
        let entry = cache.entries[slot(key)];
        if entry.tag == key | 1 {
            entry.out_path.map(CellView::new)
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn cached_derivation(&self, set: BindingsRoot<'_, 'h>) -> Option<bool> {
        let cache = self.derivation_cache.borrow();
        if cache.collection != self.raw.stats().collections || cache.entries.is_empty() {
            return None;
        }
        let key = self.bindings_identity(set).address;
        let entry = cache.entries[slot(key)];
        (entry.tag & !1 == key).then_some(entry.tag & 1 != 0)
    }

    /// Published bindings are immutable and keep their cells alive. The epoch
    /// check excludes moved/freed pointers, and the returned view borrows the
    /// heap so it cannot survive collection. Cache misses use the rooted set.
    #[inline]
    pub(crate) fn derivation_out_path<'v>(
        &'v self,
        set: BindingsRoot<'_, 'h>,
    ) -> Option<CellView<'v, 'h>> {
        let cache = self.derivation_cache.borrow();
        if cache.collection == self.raw.stats().collections && !cache.entries.is_empty() {
            let key = self.bindings_identity(set).address;
            let entry = cache.entries[slot(key)];
            if entry.tag == key | 1 {
                return entry.out_path.map(CellView::new);
            }
        }
        self.bindings_view(set)
            .get(crate::symbol::sym::OUT_PATH)
            .map(|attr| attr.value)
    }

    /// Only call after classification succeeds with a stable type value (or
    /// no type attribute). Never cache failures, suspensions or lazy hashes.
    pub(crate) fn cache_derivation(&self, set: BindingsRoot<'_, 'h>, value: bool) {
        let key = self.bindings_identity(set).address;
        debug_assert_eq!(key & 1, 0, "binding headers are aligned");
        // Looking up an attribute does not force its cell or collect.
        let out_path = if value {
            self.bindings_view(set)
                .get(crate::symbol::sym::OUT_PATH)
                .map(|attr| attr.value.pointer)
        } else {
            None
        };
        let mut cache = self.derivation_cache.borrow_mut();
        if cache.entries.is_empty() {
            cache.entries = vec![Entry::default(); SLOTS];
        }
        let collection = self.raw.stats().collections;
        if cache.collection != collection {
            cache.entries.fill(Entry::default());
            cache.collection = collection;
        }
        cache.entries[slot(key)] = Entry {
            tag: key | usize::from(value),
            out_path,
        };
    }
}
