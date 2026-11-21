//! Heap-address identity, confined to one module.
//!
//! Two regimes cover every address-derived integer in the managed heap:
//!
//! - [`Identity`] borrows the heap for `'v`, and collection requires
//!   `&mut Heap`, so an identity can never cross a collection. Compile-time.
//! - [`AddressTag`] is storable, but a cache can only reach its stored tags
//!   through an [`EpochGated`] container keyed by the collection count, which
//!   every successful collection advances. No tag is compared after the
//!   collection that could have moved or reused its address.
//!
//! The raw `usize` never leaves this module: sibling cache modules can only
//! mint, flag, hash and compare through the methods below, and the rest of
//! the crate cannot name a movable pointer type at all.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::symbol::Symbol;

/// Opaque identity valid only while its heap is immutably borrowed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Identity<'v> {
    address: usize,
    borrow: PhantomData<&'v ()>,
}

impl Identity<'_> {
    /// Minting is confined to the `Heap` identity methods, which brand the
    /// result with their caller's heap borrow; no other module handles the
    /// raw pointer. The pointer is never dereferenced here.
    #[inline]
    pub(in crate::managed_heap) fn from_pointer(pointer: NonNull<u8>) -> Self {
        Self {
            address: pointer.as_ptr() as usize,
            borrow: PhantomData,
        }
    }
}

/// A collection count observed from a live heap; mint with `Heap::epoch`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::managed_heap) struct Epoch(usize);

impl Epoch {
    #[inline]
    pub(in crate::managed_heap) fn new(collections: usize) -> Self {
        Self(collections)
    }
}

/// A storable address key. Tags are only reachable again through an
/// [`EpochGated`] store, so they are never compared across a collection.
/// `EMPTY` matches no identity: object addresses are never null.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::managed_heap) struct AddressTag(usize);

impl Default for AddressTag {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl AddressTag {
    pub(in crate::managed_heap) const EMPTY: Self = Self(0);

    #[inline]
    pub(in crate::managed_heap) fn new(identity: Identity<'_>) -> Self {
        Self(identity.address)
    }

    /// Pack a bool into bit 0; object headers are aligned, so the bit is free.
    #[inline]
    pub(in crate::managed_heap) fn with_flag(self, flag: bool) -> Self {
        debug_assert_eq!(self.0 & 1, 0, "object headers are aligned");
        Self(self.0 | usize::from(flag))
    }

    #[inline]
    pub(in crate::managed_heap) fn flag(self) -> bool {
        self.0 & 1 != 0
    }

    /// Exact address equality, for tags that never carry a flag.
    #[inline]
    pub(in crate::managed_heap) fn matches(self, identity: Identity<'_>) -> bool {
        self.0 == identity.address
    }

    /// Address equality ignoring a packed flag.
    #[inline]
    pub(in crate::managed_heap) fn matches_any_flag(self, identity: Identity<'_>) -> bool {
        self.0 & !1 == identity.address
    }

    /// Address and flag equality in one compare.
    #[inline]
    pub(in crate::managed_heap) fn matches_flagged(
        self,
        identity: Identity<'_>,
        flag: bool,
    ) -> bool {
        debug_assert_eq!(identity.address & 1, 0, "object headers are aligned");
        self.0 == identity.address | usize::from(flag)
    }

    /// Bounded-table slot. Mixes high address bits too: objects are aligned
    /// and slabs are clustered. Flag-insensitive for power-of-two masks ≥ 4
    /// because bits 0..3 are shifted out.
    #[inline]
    pub(in crate::managed_heap) fn slot(self, mask: usize) -> usize {
        let mixed = (self.0 >> 3).wrapping_mul(0x9e3779b9);
        (mixed ^ (mixed >> (usize::BITS / 2))) & mask
    }

    /// Bounded-table slot folding in a looked-up attribute name.
    #[inline]
    pub(in crate::managed_heap) fn slot_mixed(self, name: Symbol, mask: usize) -> usize {
        let mixed =
            (self.0 >> 3).wrapping_mul(0x9e3779b9) ^ (name.0 as usize).wrapping_mul(0x85ebca6b);
        (mixed ^ (mixed >> (usize::BITS / 2))) & mask
    }
}

/// Cache storage readable only at the epoch it was stamped with. Reading at
/// any other epoch yields `None`; writing at a new epoch runs `reset` first.
/// This makes "compare a stored address without the epoch check" un-writable.
#[derive(Default)]
pub(in crate::managed_heap) struct EpochGated<T> {
    collection: usize,
    inner: T,
}

impl<T> EpochGated<T> {
    #[inline]
    pub(in crate::managed_heap) fn read(&self, epoch: Epoch) -> Option<&T> {
        (self.collection == epoch.0).then_some(&self.inner)
    }

    /// Access for writing at `epoch`, running `reset` exactly once when the
    /// stamp changes (including the first write after `Default`'s epoch 0 on
    /// a heap that has already collected).
    #[inline]
    pub(in crate::managed_heap) fn write_current(
        &mut self,
        epoch: Epoch,
        reset: impl FnOnce(&mut T),
    ) -> &mut T {
        if self.collection != epoch.0 {
            reset(&mut self.inner);
            self.collection = epoch.0;
        }
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(address: usize) -> Identity<'static> {
        Identity {
            address,
            borrow: PhantomData,
        }
    }

    #[test]
    fn tags_match_flags_and_slots_stay_in_range() {
        let id = identity(0x1000);
        let tag = AddressTag::new(id);
        assert!(tag.matches(id));
        assert!(tag.matches_any_flag(id));
        assert!(tag.matches_flagged(id, false));
        assert!(!tag.matches_flagged(id, true));
        assert!(!tag.flag());
        let flagged = tag.with_flag(true);
        assert!(flagged.flag());
        assert!(!flagged.matches(id));
        assert!(flagged.matches_any_flag(id));
        assert!(flagged.matches_flagged(id, true));
        assert!(!AddressTag::EMPTY.matches(id));
        assert!(!AddressTag::EMPTY.matches_any_flag(id));
        for mask in [0xFFF, 0x3F] {
            assert!(tag.slot(mask) <= mask);
            assert!(tag.slot_mixed(Symbol(17), mask) <= mask);
            assert_eq!(tag.slot(mask), flagged.slot(mask));
        }
        assert_ne!(
            tag.slot_mixed(Symbol(17), 0xFFF),
            tag.slot_mixed(Symbol(18), 0xFFF),
        );
    }

    #[test]
    fn epoch_gating_hides_stale_stores_and_resets_once() {
        let mut gated = EpochGated::<Vec<u32>>::default();
        assert!(gated.read(Epoch::new(0)).is_some_and(Vec::is_empty));
        assert!(gated.read(Epoch::new(1)).is_none());
        gated.write_current(Epoch::new(1), |v| v.push(7)).push(1);
        assert_eq!(gated.read(Epoch::new(1)).unwrap(), &[7, 1]);
        assert!(gated.read(Epoch::new(0)).is_none());
        assert!(gated.read(Epoch::new(2)).is_none());
        // Same epoch: no reset; new epoch: reset runs exactly once.
        gated.write_current(Epoch::new(1), |v| v.clear()).push(2);
        assert_eq!(gated.read(Epoch::new(1)).unwrap(), &[7, 1, 2]);
        gated.write_current(Epoch::new(2), |v| v.clear()).push(3);
        assert_eq!(gated.read(Epoch::new(2)).unwrap(), &[3]);
    }
}
