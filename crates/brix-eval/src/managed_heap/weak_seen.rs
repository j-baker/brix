//! The weak identity set behind `Heap::mark_seen`.
//!
//! Entries are cell addresses that do not keep their cells alive. A collection
//! promotes every nursery survivor, and old-generation objects neither move nor
//! die in a minor collection. So only the addresses recorded since the previous
//! collection need rekeying at a minor collection; the settled ones are examined
//! at major collections alone. Rekeying the whole set at every collection cost
//! about 100 ms per collection on a large flake check.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

/// The address of a managed object, kept without its provenance: identity
/// sets compare addresses and never dereference them.
pub(crate) type Address = NonZeroUsize;

/// The pointer `Relocation::lookup` takes for an address.
pub(crate) fn pointer(address: Address) -> NonNull<u8> {
    NonNull::dangling().with_addr(address)
}

#[derive(Default)]
pub(crate) struct WeakSeen {
    /// Addresses that were in the old generation (or outside the managed heap)
    /// at the previous collection.
    settled: HashSet<Address>,
    /// Addresses recorded since the previous collection.
    recent: HashSet<Address>,
}

/// The destination reserved before a collection changes any root. A failed
/// collection drops it and leaves the set as it was.
pub(crate) enum Rekeyed {
    /// A minor collection: the new addresses of the surviving recent entries.
    Promoted(Vec<Address>),
    /// A major collection: the whole surviving set.
    All(HashSet<Address>),
}

impl WeakSeen {
    /// True the first time an address is recorded.
    pub fn insert(&mut self, address: Address) -> bool {
        !self.settled.contains(&address) && self.recent.insert(address)
    }

    pub fn len(&self) -> usize {
        self.settled.len() + self.recent.len()
    }

    /// `collects_old`: whether the coming collection is a major one, as
    /// requested or because the old generation reached its budget.
    pub fn prepare(&mut self, collects_old: bool) -> Rekeyed {
        if collects_old {
            Rekeyed::All(HashSet::with_capacity(self.len()))
        } else {
            self.settled.reserve(self.recent.len());
            Rekeyed::Promoted(Vec::with_capacity(self.recent.len()))
        }
    }

    /// Called from a collection's weak-update callback, for the collection
    /// `rekeyed` was prepared for.
    pub fn relocate(&self, rekeyed: &mut Rekeyed, relocation: &brix_gc::raw::Relocation<'_>) {
        let lookup = |address: Address| relocation.lookup(pointer(address)).map(NonNull::addr);
        match rekeyed {
            Rekeyed::Promoted(promoted) => {
                // Settled entries left alone through a major collection would
                // name dead or moved cells.
                assert!(!relocation.collects_old());
                promoted.extend(self.recent.iter().copied().filter_map(lookup));
            }
            Rekeyed::All(all) => {
                all.extend(self.settled.iter().copied().filter_map(lookup));
                all.extend(self.recent.iter().copied().filter_map(lookup));
            }
        }
    }

    pub fn commit(&mut self, rekeyed: Rekeyed) {
        self.recent.clear();
        match rekeyed {
            Rekeyed::Promoted(promoted) => self.settled.extend(promoted),
            Rekeyed::All(all) => self.settled = all,
        }
    }
}
