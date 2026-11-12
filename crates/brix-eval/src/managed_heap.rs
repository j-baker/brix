//! Safe, rooted access to the moving evaluator heap.
//!
//! This is the heap interface used by the evaluator migration, not an adapter
//! which makes the existing arena references movable. Roots survive allocation;
//! views borrow the heap and therefore exclude allocation, mutation and GC.

#![allow(unsafe_code)]
#![allow(dead_code)] // Evaluator callers are being migrated separately.

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use brix_gc::raw::{RawHeap, Tracer};
#[cfg(brix_parallel)]
#[path = "parallel/heap.rs"]
mod parallel_heap;
#[path = "managed_heap/weak_seen.rs"]
mod weak_seen;
pub(crate) use weak_seen::{Address, WeakSeen};
use brix_gc::roots::{self as raw_roots, RootDescriptor, RootKind, RootRegistry};
use brix_gc::{Config, Error, Result, Stats};
use brix_store::AccessorId;
use smallvec::SmallVec;

use crate::heap_layout::*;
use crate::symbol::Symbol;
use crate::value::{GlobalNode, SrcPos};

type Brand<'h> = PhantomData<fn(&'h ()) -> &'h ()>;

macro_rules! root_kind {
    ($name:ident, $stored:ty, $kind:expr) => {
        pub(crate) enum $name {}
        // SAFETY: ROOT_DESCRIPTORS has the exact Stored layout and tracer.
        unsafe impl RootKind for $name {
            type Stored = $stored;
            const KIND: usize = $kind;
        }
    };
}
root_kind!(ValueKind, RawValue, 0);
root_kind!(CellKind, CellPtr, 1);
root_kind!(EnvKind, NonNull<RawEnv>, 2);
root_kind!(BindingsKind, NonNull<RawBindings>, 3);
root_kind!(BytesKind, RawSlice<u8>, 4);
root_kind!(ContextKind, RawSlice<RawContext>, 5);
root_kind!(NodeKind, NonNull<RawNode>, 6);
root_kind!(AttrsKind, RawSlice<RawAttr>, 7);
root_kind!(NodesKind, RawSlice<NonNull<RawNode>>, 8);

unsafe fn trace_value(pointer: NonNull<u8>, tracer: &mut Tracer<'_>) {
    // RawCell is repr(C) with RawValue as its sole field at offset zero.
    unsafe { (DESCRIPTORS[CELL].trace)(pointer, size_of::<RawValue>(), tracer) };
}
unsafe fn trace_pointer(pointer: NonNull<u8>, tracer: &mut Tracer<'_>) {
    unsafe {
        let slot = pointer.cast::<NonNull<u8>>().as_ptr();
        slot.write(tracer.relocate(slot.read()));
    }
}
unsafe fn trace_slice<T>(pointer: NonNull<u8>, tracer: &mut Tracer<'_>) {
    unsafe {
        let slot = pointer.cast::<RawSlice<T>>().as_ptr();
        let mut slice = slot.read();
        if slice.len != 0 {
            slice.ptr = tracer.relocate(slice.ptr.cast()).cast();
        } else {
            slice = RawSlice::empty();
        }
        slot.write(slice);
    }
}
const fn descriptor<T>(trace: unsafe fn(NonNull<u8>, &mut Tracer<'_>)) -> RootDescriptor {
    RootDescriptor {
        size: size_of::<T>(),
        align: align_of::<T>(),
        trace,
    }
}
static ROOT_DESCRIPTORS: [RootDescriptor; 9] = [
    descriptor::<RawValue>(trace_value),
    descriptor::<CellPtr>(trace_pointer),
    descriptor::<NonNull<RawEnv>>(trace_pointer),
    descriptor::<NonNull<RawBindings>>(trace_pointer),
    descriptor::<RawSlice<u8>>(trace_slice::<u8>),
    descriptor::<RawSlice<RawContext>>(trace_slice::<RawContext>),
    descriptor::<NonNull<RawNode>>(trace_pointer),
    descriptor::<RawSlice<RawAttr>>(trace_slice::<RawAttr>),
    descriptor::<RawSlice<NonNull<RawNode>>>(trace_slice::<NonNull<RawNode>>),
];

/// Copyable alias of a root slot; its raw payload is never exposed.
pub(crate) struct Root<'r, 'h, K: RootKind> {
    raw: raw_roots::RawRoot<'r, 'h, K>,
}
impl<K: RootKind> Copy for Root<'_, '_, K> {}
impl<K: RootKind> Clone for Root<'_, '_, K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: RootKind> std::fmt::Debug for Root<'_, '_, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Root(..)")
    }
}
pub(crate) type ValueRoot<'r, 'h> = Root<'r, 'h, ValueKind>;
pub(crate) type CellRoot<'r, 'h> = Root<'r, 'h, CellKind>;
pub(crate) type EnvRoot<'r, 'h> = Root<'r, 'h, EnvKind>;
pub(crate) type BindingsRoot<'r, 'h> = Root<'r, 'h, BindingsKind>;
pub(crate) type BytesRoot<'r, 'h> = Root<'r, 'h, BytesKind>;
pub(crate) type ContextRoot<'r, 'h> = Root<'r, 'h, ContextKind>;
pub(crate) type NodeRoot<'r, 'h> = Root<'r, 'h, NodeKind>;

pub(crate) struct Slot<'r, 'h, K: RootKind> {
    raw: raw_roots::RawSlot<'r, 'h, K>,
}
impl<K: RootKind> Copy for Slot<'_, '_, K> {}
impl<K: RootKind> Clone for Slot<'_, '_, K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'r, 'h, K: RootKind> Slot<'r, 'h, K> {
    pub fn root(self) -> Root<'r, 'h, K> {
        Root {
            raw: self.raw.root(),
        }
    }
    pub fn set(self, value: Root<'_, 'h, K>) {
        unsafe { self.raw.write(value.raw.read()) }
    }
}

/// Persistent root registration. Last-lease drop removes its strong reachability.
pub(crate) struct Lease<'h, K: RootKind> {
    #[cfg(not(brix_parallel))]
    raw: raw_roots::Persistent<'h, K>,
    #[cfg(brix_parallel)]
    raw: raw_roots::shared::Persistent<'h, K>,
}
impl<K: RootKind> Clone for Lease<'_, K> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
        }
    }
}
impl<'h, K: RootKind> Lease<'h, K> {
    pub fn root(&self) -> Root<'_, 'h, K> {
        Root {
            raw: self.raw.as_root(),
        }
    }
    pub fn set(&self, value: Root<'_, 'h, K>) {
        unsafe { self.raw.write(value.raw.read()) }
    }
}

pub(crate) struct Roots<'r, 'h> {
    raw: raw_roots::Roots<'r, 'h>,
}
impl<'r, 'h> Roots<'r, 'h> {
    /// Enter a nested scope through an existing registry capability. The outer
    /// frame keeps the registry alive, so this needs no owning Rc clone.
    pub fn with_roots<R>(&self, callback: impl for<'inner> FnOnce(&Roots<'inner, 'h>) -> R) -> R {
        self.raw.with_roots(|raw| callback(&Roots { raw }))
    }

    fn push<K: RootKind>(&self, value: K::Stored) -> Root<'r, 'h, K> {
        Root {
            raw: unsafe { self.raw.push::<K>(value) },
        }
    }
    pub fn int(&self, value: i64) -> ValueRoot<'r, 'h> {
        self.push::<ValueKind>(RawValue::Int(value))
    }
    pub fn float(&self, value: f64) -> ValueRoot<'r, 'h> {
        self.push::<ValueKind>(RawValue::Float(value))
    }
    pub fn bool(&self, value: bool) -> ValueRoot<'r, 'h> {
        self.push::<ValueKind>(RawValue::Bool(value))
    }
    pub fn null(&self) -> ValueRoot<'r, 'h> {
        self.push::<ValueKind>(RawValue::Null)
    }
    pub fn blackhole(&self) -> ValueRoot<'r, 'h> {
        self.push::<ValueKind>(RawValue::Blackhole)
    }
    pub fn empty_context(&self) -> ContextRoot<'r, 'h> {
        self.push::<ContextKind>(RawSlice::empty())
    }
    pub fn copy<K: RootKind>(&self, value: Root<'_, 'h, K>) -> Root<'r, 'h, K> {
        self.push::<K>(value.raw.read())
    }
    /// A distinct root slot holding the value at this instant, rather than an
    /// alias of an updateable input slot.
    pub fn snapshot<K: RootKind>(&self, value: Root<'_, 'h, K>) -> Root<'r, 'h, K> {
        self.copy(value)
    }
    pub fn slot<K: RootKind>(&self, value: Root<'_, 'h, K>) -> Slot<'r, 'h, K> {
        Slot {
            raw: unsafe { self.raw.slot::<K>(value.raw.read()) },
        }
    }
    pub fn vector<K: RootKind>(&self) -> RootVec<'r, 'h, K> {
        RootVec {
            raw: self.raw.vector::<K>(),
        }
    }
}

pub(crate) struct RootVec<'r, 'h, K: RootKind> {
    raw: raw_roots::RootVec<'r, 'h, K>,
}
impl<K: RootKind> Copy for RootVec<'_, '_, K> {}
impl<K: RootKind> Clone for RootVec<'_, '_, K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'h, K: RootKind> RootVec<'_, 'h, K> {
    pub fn len(&self) -> usize {
        self.raw.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn push(&self, value: Root<'_, 'h, K>) {
        unsafe { self.raw.push(value.raw.read()) }
    }
    pub fn get<'r>(&self, roots: &Roots<'r, 'h>, index: usize) -> Option<Root<'r, 'h, K>> {
        self.raw.get(index).map(|value| roots.push::<K>(value))
    }
    pub fn get_into(&self, slot: Slot<'_, 'h, K>, index: usize) -> bool {
        match self.raw.get(index) {
            Some(value) => {
                unsafe { slot.raw.write(value) };
                true
            }
            None => false,
        }
    }
    pub fn set(&self, index: usize, value: Root<'_, 'h, K>) {
        unsafe { self.raw.set(index, value.raw.read()) }
    }
    pub fn clear(&self) {
        self.raw.clear()
    }
}

pub(crate) struct Heap<'h> {
    #[cfg(brix_parallel)]
    storage: std::sync::Arc<parallel_heap::Storage<'h>>,
    #[cfg(brix_parallel)]
    shared: std::sync::Arc<parallel_heap::Shared<'h>>,
    #[cfg(brix_parallel)]
    mutator: crate::parallel::world::Mutator,
    #[cfg(not(brix_parallel))]
    raw: RawHeap,
    #[cfg(not(brix_parallel))]
    registry: Rc<RootRegistry<'h>>,
    #[cfg(not(brix_parallel))]
    weak_seen: WeakSeen,
    #[cfg(not(brix_parallel))]
    visited: Vec<Weak<RefCell<HashSet<Address>>>>,
    empty_attrs: RefCell<Option<Lease<'h, BindingsKind>>>,
    empty_string: Option<Lease<'h, ValueKind>>,
    empty_list: Option<Lease<'h, ValueKind>>,
    stress: bool,
    derivation_cache: RefCell<derivation_cache::DerivationCache>,
    attribute_cache: RefCell<attribute_cache::AttributeCache>,
    #[cfg(feature = "work-counters")]
    attribute_probe_reuse: RefCell<attribute_cache::AttributeProbeReuse>,
    #[cfg(feature = "work-counters")]
    probe_reuse: RefCell<equality_profile::ProbeReuse>,
    brand: Brand<'h>,
}

pub(crate) fn with_heap<R>(
    config: Config,
    callback: impl for<'h> FnOnce(&mut Heap<'h>) -> R,
) -> Result<R> {
    with_owned_heap(config, |mut heap| callback(&mut heap))
}

pub(crate) fn with_owned_heap<R>(
    config: Config,
    callback: impl for<'h> FnOnce(Heap<'h>) -> R,
) -> Result<R> {
    // The higher-ranked callback creates a fresh, non-escaping heap brand.
    let heap = unsafe { new_for_brand(config)? };
    Ok(callback(heap))
}

#[cfg(not(brix_parallel))]
unsafe fn new_for_brand<'h>(config: Config) -> Result<Heap<'h>> {
    let registry = Rc::new(unsafe { RootRegistry::new(&ROOT_DESCRIPTORS) });
    let mut raw = RawHeap::new(config, &DESCRIPTORS)?;
    // SAFETY: heap_layout callbacks inspect/rewrite only their own payload;
    // outgoing managed pointers are relocated without dereferencing them.
    // RawError's Rc is untouched by tracing and dropped on the caller thread.
    unsafe { raw.enable_parallel_tracing() };
    let heap = Heap {
        raw,
        registry,
        weak_seen: WeakSeen::default(),
        visited: Vec::new(),
        empty_attrs: RefCell::new(None),
        empty_string: None,
        empty_list: None,
        stress: config.stress,
        derivation_cache: RefCell::default(),
        attribute_cache: RefCell::default(),
        #[cfg(feature = "work-counters")]
        attribute_probe_reuse: RefCell::default(),
        #[cfg(feature = "work-counters")]
        probe_reuse: RefCell::default(),
        brand: PhantomData,
    };
    Ok(heap)
}

#[cfg(brix_parallel)]
unsafe fn new_for_brand<'h>(config: Config) -> Result<Heap<'h>> {
    unsafe { parallel_heap::new(config, 1) }
}

impl Heap<'static> {
    /// Create a heap whose brand is hidden by an owning public runtime.
    ///
    /// # Safety
    /// No roots, scopes, leases or borrowed views may be exposed directly.
    /// Every public handle must carry its runtime's unique identity, checked
    /// before any heap access. In particular, handles of two owned heaps have
    /// the same internal lifetime and must never be mixed.
    pub(crate) unsafe fn new_owned(config: Config) -> Result<Self> {
        unsafe { new_for_brand(config) }
    }
}

#[derive(Clone)]
pub(crate) struct RootScopes<'h> {
    registry: Rc<RootRegistry<'h>>,
}
impl<'h> RootScopes<'h> {
    pub fn with_roots<R>(&self, callback: impl for<'r> FnOnce(&Roots<'r, 'h>) -> R) -> R {
        self.registry.with_roots(|raw| callback(&Roots { raw }))
    }
}

/// An operation-local identity set. Entries follow live cells through moves,
/// but do not keep those cells alive. Dropping the set ends the traversal.
pub(crate) struct Visited<'h> {
    cells: Rc<RefCell<HashSet<Address>>>,
    brand: Brand<'h>,
}

/// Opaque identity valid only while its heap is immutably borrowed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Identity<'v> {
    address: usize,
    borrow: PhantomData<&'v ()>,
}

impl<'h> Heap<'h> {
    /// Leave the managed graph while an operation on owned data blocks. The
    /// exclusive borrow excludes live heap views; roots remain registered.
    #[inline]
    pub fn blocking<R>(&mut self, operation: impl FnOnce() -> R) -> R {
        #[cfg(brix_parallel)]
        let _parked = self.mutator.park();
        operation()
    }

    pub fn root_registry(&self) -> RootScopes<'h> {
        RootScopes {
            registry: self.registry.clone(),
        }
    }
    pub fn with_roots<R>(
        &mut self,
        callback: impl for<'r> FnOnce(&mut Self, &Roots<'r, 'h>) -> R,
    ) -> R {
        let registry = self.registry.clone();
        registry.with_roots(|raw| callback(self, &Roots { raw }))
    }
    pub fn persist<K: RootKind>(&self, root: Root<'_, 'h, K>) -> Lease<'h, K>
    where
        K::Stored: 'h,
    {
        Lease {
            #[cfg(not(brix_parallel))]
            raw: unsafe { self.registry.persist::<K>(root.raw.read()) },
            #[cfg(brix_parallel)]
            raw: unsafe { self.shared.roots.persist::<K>(root.raw.read()) },
        }
    }
    pub fn stats(&self) -> &Stats {
        self.raw.stats()
    }
    pub fn used_bytes(&self) -> usize {
        self.raw.used_bytes()
    }
    pub fn capacity_bytes(&self) -> usize {
        self.raw.capacity_bytes()
    }
    pub fn space_usage(&self) -> Vec<(&'static str, usize, usize)> {
        self.raw.space_usage()
    }
    pub fn root_slot_bytes(&self) -> usize {
        self.registry.reserved_slot_bytes()
    }
    pub fn collect(&mut self) -> Result<()> {
        self.collect_generation(true)
    }
    fn collect_minor(&mut self) -> Result<()> {
        self.collect_generation(false)
    }
    #[cfg(not(brix_parallel))]
    fn collect_generation(&mut self, major: bool) -> Result<()> {
        let mut seen = self
            .weak_seen
            .prepare(major || self.raw.minor_collects_old());
        // Reserve every destination set before the collector mutates roots.
        self.visited.retain(|set| set.strong_count() != 0);
        let mut visits: Vec<_> = self
            .visited
            .iter()
            .filter_map(Weak::upgrade)
            .map(|set| {
                let updated = HashSet::with_capacity(set.borrow().len());
                (set, updated)
            })
            .collect();
        let registry = &self.registry;
        // All public views borrow self; no payload reference can survive this
        // exclusive borrow. Registry callbacks are fixed private descriptors.
        let result = unsafe {
            let trace = |tracer: &mut Tracer<'_>| registry.trace_roots(tracer);
            let weak_seen = &self.weak_seen;
            let update = |relocation: &brix_gc::raw::Relocation<'_>| {
                weak_seen.relocate(&mut seen, relocation);
                for (set, updated) in &mut visits {
                    for &address in set.borrow().iter() {
                        if let Some(pointer) = relocation.lookup(weak_seen::pointer(address)) {
                            updated.insert(pointer.addr());
                        }
                    }
                }
            };
            if major {
                self.raw.collect(trace, update)
            } else {
                self.raw.collect_minor(trace, update)
            }
        };
        if result.is_ok() {
            self.weak_seen.commit(seen);
            for (set, updated) in visits {
                *set.borrow_mut() = updated;
            }
        }
        result
    }
    pub fn visited(&mut self) -> Visited<'h> {
        // Also prune here so repeated operations without GC do not accumulate
        // dead weak registrations.
        self.visited.retain(|set| set.strong_count() != 0);
        let cells = Rc::new(RefCell::new(HashSet::new()));
        self.visited.push(Rc::downgrade(&cells));
        Visited {
            cells,
            brand: PhantomData,
        }
    }
    pub fn mark_visited(&self, visited: &Visited<'h>, cell: CellRoot<'_, 'h>) -> bool {
        visited
            .cells
            .borrow_mut()
            .insert(cell.raw.read().addr())
    }
    /// Returns true the first time this cell is seen. Dead entries do not retain
    /// cells and disappear at collection; survivors are rekeyed after movement.
    pub fn mark_seen(&mut self, cell: CellRoot<'_, 'h>) -> bool {
        self.weak_seen
            .insert(cell.raw.read().addr())
    }
    pub fn mark_bindings_visited(
        &self,
        visited: &Visited<'h>,
        bindings: BindingsRoot<'_, 'h>,
    ) -> bool {
        visited
            .cells
            .borrow_mut()
            .insert(bindings.raw.read().addr())
    }
    pub fn cell_identity(&self, cell: CellRoot<'_, 'h>) -> Identity<'_> {
        Identity {
            address: cell.raw.read().as_ptr() as usize,
            borrow: PhantomData,
        }
    }
    pub fn bindings_identity(&self, bindings: BindingsRoot<'_, 'h>) -> Identity<'_> {
        Identity {
            address: bindings.raw.read().as_ptr() as usize,
            borrow: PhantomData,
        }
    }
    pub fn seen_count(&self) -> usize {
        self.weak_seen.len()
    }
    pub fn same_cell(&self, a: CellRoot<'_, 'h>, b: CellRoot<'_, 'h>) -> bool {
        a.raw.read() == b.raw.read()
    }
    #[inline]
    pub(crate) fn cell_view<'v>(&'v self, cell: CellRoot<'_, 'h>) -> CellView<'v, 'h> {
        CellView::new(cell.raw.read())
    }
    #[inline]
    pub(crate) fn same_cell_views(&self, a: CellView<'_, 'h>, b: CellView<'_, 'h>) -> bool {
        a.pointer == b.pointer
    }
    pub fn same_bindings(&self, a: BindingsRoot<'_, 'h>, b: BindingsRoot<'_, 'h>) -> bool {
        a.raw.read() == b.raw.read()
    }
    pub fn same_node(&self, a: NodeRoot<'_, 'h>, b: NodeRoot<'_, 'h>) -> bool {
        a.raw.read() == b.raw.read()
    }
    // On return the caller must initialize and publish this allocation before
    // any further call that can collect. All constructors below do exactly so.
    #[inline]
    fn reserve(&mut self, kind: usize, bytes: usize) -> Result<NonNull<u8>> {
        if !self.stress {
            if let Some(pointer) = unsafe { self.raw.try_allocate(kind, bytes) } {
                return Ok(pointer);
            }
        }
        self.reserve_slow(kind, bytes)
    }

    // SAFETY: kind must describe a nonzero fixed-size payload with physical
    // extent BYTES and no prefix. As with reserve, initialize and publish the
    // allocation before another collecting call, reloading all rooted inputs.
    #[inline]
    unsafe fn reserve_fixed<const BYTES: usize>(&mut self, kind: usize) -> Result<NonNull<u8>> {
        if !self.stress {
            // SAFETY: this helper's caller guarantees the descriptor contract.
            if let Some(pointer) = unsafe { self.raw.try_allocate_known_fixed::<BYTES>(kind) } {
                return Ok(pointer);
            }
        }
        self.reserve_slow(kind, BYTES)
    }

    #[cold]
    #[inline(never)]
    fn reserve_slow(&mut self, kind: usize, bytes: usize) -> Result<NonNull<u8>> {
        let extent = self.raw.allocation_extent(kind, bytes)?;
        // RawHeap owns the same immutable stress setting and includes it in
        // collection_due; repeating the wrapper's check adds no safeguard.
        if self.raw.collection_due(extent) {
            self.collect_minor()?;
        }
        #[cfg(test)]
        if FAIL_SLOW_RESERVATION.get() {
            return Err(Error::OutOfMemory);
        }
        unsafe { self.raw.allocate_slow(kind, bytes) }
    }
    pub fn view<'v>(&'v self, value: ValueRoot<'_, 'h>) -> ValueView<'v, 'h> {
        self.decode(value.raw.read())
    }
    pub fn read<'v>(&'v self, cell: CellRoot<'_, 'h>) -> ValueView<'v, 'h> {
        self.decode(unsafe { cell.raw.read().as_ref().load() })
    }
    /// Read and classify once for callers whose fast path only accepts WHNF.
    #[inline]
    pub(crate) fn read_whnf<'v>(&'v self, cell: CellRoot<'_, 'h>) -> Option<ValueView<'v, 'h>> {
        let value = unsafe { cell.raw.read().as_ref().load_for_whnf() };
        match value {
            RawValue::Thunk { .. }
            | RawValue::App(_)
            | RawValue::Blackhole
            | RawValue::Failed(_) => None,
            _ => Some(self.decode(value)),
        }
    }
    /// Inspect only the cell tag; no borrowed view or snapshot root is needed.
    #[inline]
    pub fn is_whnf(&self, cell: CellRoot<'_, 'h>) -> bool {
        !matches!(
            unsafe { cell.raw.read().as_ref().load_for_whnf() },
            RawValue::Thunk { .. } | RawValue::App(_) | RawValue::Blackhole | RawValue::Failed(_)
        )
    }
    /// Borrowed-cell counterpart of `is_whnf`: inspect the tag without
    /// decoding a value view or registering another root.
    #[inline]
    pub(crate) fn is_whnf_view(&self, cell: CellView<'_, 'h>) -> bool {
        !matches!(
            unsafe { cell.pointer.as_ref().load_for_whnf() },
            RawValue::Thunk { .. } | RawValue::App(_) | RawValue::Blackhole | RawValue::Failed(_)
        )
    }
    pub fn read_view<'v>(&'v self, cell: CellView<'v, 'h>) -> ValueView<'v, 'h> {
        self.decode(unsafe { cell.pointer.as_ref().load() })
    }
    pub fn value<'r>(&self, roots: &Roots<'r, 'h>, cell: CellRoot<'_, 'h>) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(unsafe { cell.raw.read().as_ref().load() })
    }
    /// Snapshot a borrowed cell directly into an independently traced value.
    /// Root registration does not collect; the view's heap borrow keeps its
    /// address valid until the copied payload has been published to the roots.
    pub fn value_from_view<'r, 'v>(
        &'v self,
        roots: &Roots<'r, 'h>,
        cell: CellView<'v, 'h>,
    ) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(unsafe { cell.pointer.as_ref().load() })
    }
    #[inline]
    pub fn cell<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        value: ValueRoot<'_, 'h>,
    ) -> Result<CellRoot<'r, 'h>> {
        // SAFETY: CELL describes RawCell, whose Rust size includes alignment.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawCell>() }>(CELL)? }.cast::<RawCell>();
        unsafe { pointer.as_ptr().write(RawCell::new(value.raw.read())) };
        Ok(roots.push::<CellKind>(pointer))
    }
    /// Allocate a suspension without rooting an intermediate value payload.
    #[inline]
    pub fn thunk_cell<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        node: GlobalNode,
    ) -> Result<CellRoot<'r, 'h>> {
        // SAFETY: CELL describes RawCell, whose Rust size includes alignment.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawCell>() }>(CELL)? }.cast::<RawCell>();
        // Reservation may collect; reload the environment from its root after it.
        unsafe {
            pointer.as_ptr().write(RawCell::new(RawValue::Thunk {
                env: env.raw.read(),
                node,
            }));
        }
        Ok(roots.push::<CellKind>(pointer))
    }
    #[inline]
    #[cfg(not(brix_parallel))]
    pub fn store(&mut self, cell: CellRoot<'_, 'h>, value: ValueRoot<'_, 'h>) {
        let pointer = cell.raw.read();
        let value = value.raw.read();
        // Immediate values cannot introduce an old-to-young edge. Removing
        // an edge needs no barrier; an existing dirty bit remains conservative.
        if !matches!(
            value,
            RawValue::Int(_)
                | RawValue::Float(_)
                | RawValue::Bool(_)
                | RawValue::Null
                | RawValue::PrimOp(_)
                | RawValue::Blackhole
        ) {
            self.raw.write_barrier(pointer.cast());
        }
        unsafe { pointer.as_ptr().write(RawCell::new(value)) };
    }
    /// Restore a thunk from its rooted environment and immutable code ID.
    /// This does not allocate, collect, or require an intermediate value root.
    #[inline]
    #[cfg(not(brix_parallel))]
    pub fn store_thunk(&mut self, cell: CellRoot<'_, 'h>, env: EnvRoot<'_, 'h>, node: GlobalNode) {
        self.raw.write_barrier(cell.raw.read().cast());
        unsafe {
            cell.raw
                .read()
                .as_ptr()
                .write(RawCell::new(RawValue::Thunk {
                    env: env.raw.read(),
                    node,
                }));
        }
    }
    /// Blackholing writes an immediate tag and cannot collect; no temporary
    /// value root is required. The caller roots the saved suspension separately.
    #[inline]
    #[cfg(not(brix_parallel))]
    pub fn store_blackhole(&mut self, cell: CellRoot<'_, 'h>) {
        unsafe {
            cell.raw
                .read()
                .as_ptr()
                .write(RawCell::new(RawValue::Blackhole));
        }
    }
    pub fn bytes<'r>(&mut self, roots: &Roots<'r, 'h>, bytes: &[u8]) -> Result<BytesRoot<'r, 'h>> {
        if bytes.is_empty() {
            return Ok(roots.push::<BytesKind>(RawSlice::empty()));
        }
        let pointer = self.reserve(BYTES, bytes.len())?;
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr(), bytes.len()) };
        Ok(roots.push::<BytesKind>(RawSlice::new(pointer, bytes.len())))
    }
    /// Copy rooted string parts directly into one final byte allocation. Every
    /// source is reread after reserve, which may relocate the entire input graph.
    pub fn concat_string_bytes<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        parts: &[ValueRoot<'_, 'h>],
    ) -> Result<BytesRoot<'r, 'h>> {
        let total = self.concat_byte_length(parts)?;
        if total == 0 {
            return Ok(roots.push::<BytesKind>(RawSlice::empty()));
        }
        let pointer = self.reserve(BYTES, total)?;
        unsafe { self.fill_concat_bytes(parts, pointer) };
        Ok(roots.push::<BytesKind>(RawSlice::new(pointer, total)))
    }
    fn concat_byte_length(&self, parts: &[ValueRoot<'_, 'h>]) -> Result<usize> {
        let mut total = 0usize;
        for &part in parts {
            let ValueView::String(string) = self.view(part) else {
                return Err(Error::InvalidRange);
            };
            total = total
                .checked_add(string.bytes().len())
                .ok_or(Error::AllocationTooLarge)?;
        }
        Ok(total)
    }

    // SAFETY: pointer must have room for the validated total byte length and
    // must not overlap any source. No collection occurs inside this helper.
    unsafe fn fill_concat_bytes(&self, parts: &[ValueRoot<'_, 'h>], pointer: NonNull<u8>) {
        let mut offset = 0;
        // No allocation or callback occurs until the bytes are initialized and
        // registered. Fresh output storage cannot overlap any source allocation.
        for &part in parts {
            let ValueView::String(string) = self.view(part) else {
                unreachable!("validated string part")
            };
            let bytes = string.bytes();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    pointer.as_ptr().add(offset),
                    bytes.len(),
                );
            }
            offset += bytes.len();
        }
    }

    /// Build a concatenation with one collection boundary and one result root.
    /// All inputs remain rooted; each intermediate payload is fully initialized
    /// before the next allocation, and raw refill never invokes collection.
    pub fn concat_strings<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        parts: &[ValueRoot<'_, 'h>],
        segments: &[(ValueRoot<'_, 'h>, usize)],
    ) -> Result<ValueRoot<'r, 'h>> {
        let total = self.concat_byte_length(parts)?;
        let count = self.validate_context_segments(segments)?;
        let empty = total == 0 && count == 0;
        if empty {
            if let Some(value) = &self.empty_string {
                return Ok(roots.copy(value.root()));
            }
        }
        let mut pending = size_of::<RawStr>();
        if total != 0 {
            pending = pending
                .checked_add(self.raw.allocation_extent(BYTES, total)?)
                .ok_or(Error::AllocationTooLarge)?;
        }
        if count != 0 {
            pending = pending
                .checked_add(
                    self.raw
                        .allocation_extent(CONTEXTS, array_bytes::<RawContext>(count)?)?,
                )
                .ok_or(Error::AllocationTooLarge)?;
        }
        let mut context = SmallVec::<[RawContext; 4]>::new();
        context.try_reserve(count).map_err(|_| Error::OutOfMemory)?;
        if self.raw.collection_due(pending) {
            self.collect_minor()?;
        }
        // From here until publication, all pointer-bearing scratch is private
        // and no call can collect. Reload source edges after the preflight GC.
        self.fill_context_segments(segments, &mut context);
        let bytes = if total == 0 {
            RawSlice::empty()
        } else {
            #[cfg(test)]
            concat_allocation_checkpoint()?;
            let pointer = unsafe {
                match self.raw.try_allocate(BYTES, total) {
                    Some(pointer) => pointer,
                    None => self.raw.allocate_slow(BYTES, total)?,
                }
            };
            unsafe { self.fill_concat_bytes(parts, pointer) };
            RawSlice::new(pointer, total)
        };
        let context = unsafe { self.allocate_contexts_uncollected(&context)? };
        #[cfg(test)]
        concat_allocation_checkpoint()?;
        // SAFETY: STRING is a sealed fixed descriptor with exactly this extent.
        let pointer = unsafe {
            match self
                .raw
                .try_allocate_known_fixed::<{ size_of::<RawStr>() }>(STRING)
            {
                Some(pointer) => pointer,
                None => self.raw.allocate_slow(STRING, size_of::<RawStr>())?,
            }
        }
        .cast::<RawStr>();
        unsafe { pointer.as_ptr().write(RawStr { bytes, context }) };
        let value = roots.push::<ValueKind>(RawValue::String(pointer));
        if empty {
            self.empty_string = Some(self.persist(value));
        }
        Ok(value)
    }

    pub fn bytes_view<'v>(&'v self, bytes: BytesRoot<'_, 'h>) -> &'v [u8] {
        unsafe { slice(bytes.raw.read()) }
    }
    /// Combine context edges without copying their byte allocations. Segments
    /// supply the output byte offset of each rooted string; context-only inputs
    /// (such as a separator retained for an empty list) can use offset zero.
    /// Callers that evaluate segments incrementally must also validate offsets
    /// before evaluating later segments, to preserve language error ordering.
    pub fn concat_string_contexts<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        segments: &[(ValueRoot<'_, 'h>, usize)],
    ) -> Result<ContextRoot<'r, 'h>> {
        let count = self.validate_context_segments(segments)?;
        if count == 0 {
            return Ok(roots.empty_context());
        }
        let pending = self
            .raw
            .allocation_extent(CONTEXTS, array_bytes::<RawContext>(count)?)?;
        let mut context = smallvec::SmallVec::<[RawContext; 4]>::new();
        context.try_reserve(count).map_err(|_| Error::OutOfMemory)?;
        if self.raw.collection_due(pending) {
            self.collect_minor()?;
        }

        self.fill_context_segments(segments, &mut context);
        let context = unsafe { self.allocate_contexts_uncollected(&context)? };
        Ok(roots.push::<ContextKind>(context))
    }

    fn validate_context_segments(&self, segments: &[(ValueRoot<'_, 'h>, usize)]) -> Result<usize> {
        let mut count = 0usize;
        for &(value, prefix) in segments {
            let ValueView::String(string) = self.view(value) else {
                return Err(Error::InvalidRange);
            };
            let context = unsafe { slice(string.pointer.as_ref().context) };
            count = count
                .checked_add(context.len())
                .ok_or(Error::AllocationTooLarge)?;
            for entry in context {
                if let RawContext::LazyHash { offset, .. } = entry {
                    u32::try_from(prefix)
                        .ok()
                        .and_then(|prefix| offset.checked_add(prefix))
                        .ok_or(Error::AllocationTooLarge)?;
                }
            }
        }
        Ok(count)
    }

    fn fill_context_segments(
        &self,
        segments: &[(ValueRoot<'_, 'h>, usize)],
        context: &mut SmallVec<[RawContext; 4]>,
    ) {
        // All managed pointers are loaded after the only collection point.
        // Scratch has enough capacity, sorting invokes only this private byte
        // comparator, and raw allocation below cannot collect or call user code.
        for &(value, prefix) in segments {
            let ValueView::String(string) = self.view(value) else {
                unreachable!("validated string segment")
            };
            for &entry in unsafe { slice(string.pointer.as_ref().context) } {
                context.push(match entry {
                    RawContext::LazyHash { offset, tree } => RawContext::LazyHash {
                        offset: offset + prefix as u32,
                        tree,
                    },
                    entry => entry,
                });
            }
        }
        // The variant and field order deliberately matches OwnedContext and
        // ContextElem. Byte contents, not pointer identity, determine equality.
        #[derive(PartialEq, Eq, PartialOrd, Ord)]
        enum Key<'a> {
            LazyHash(u32, u32),
            LazyPath(u32),
            Opaque(&'a [u8]),
            DrvDeep(&'a [u8]),
            Built(&'a [u8], &'a [u8]),
            Path(&'a [u8]),
        }
        // SAFETY: only initialized contexts reloaded from rooted segments enter
        // this helper; no collecting call occurs while their slices are used.
        unsafe fn key(entry: &RawContext) -> Key<'_> {
            unsafe {
                match *entry {
                    RawContext::LazyHash { offset, tree } => Key::LazyHash(offset, tree),
                    RawContext::LazyPath(tree) => Key::LazyPath(tree),
                    RawContext::Opaque(path) => Key::Opaque(slice(path)),
                    RawContext::DrvDeep(path) => Key::DrvDeep(slice(path)),
                    RawContext::Built { drv, output } => Key::Built(slice(drv), slice(output)),
                    RawContext::Path(path) => Key::Path(slice(path)),
                }
            }
        }
        context.sort_unstable_by(|a, b| unsafe { key(a).cmp(&key(b)) });
        context.dedup_by(|a, b| unsafe { key(a) == key(b) });
    }

    // SAFETY: context edges must belong to this heap and remain current; no
    // collection may occur before the returned slice is rooted or published.
    unsafe fn allocate_contexts_uncollected(
        &mut self,
        context: &[RawContext],
    ) -> Result<RawSlice<RawContext>> {
        if context.is_empty() {
            return Ok(RawSlice::empty());
        }
        let bytes = array_bytes::<RawContext>(context.len())?;
        #[cfg(test)]
        concat_allocation_checkpoint()?;
        #[cfg(test)]
        if FAIL_SLOW_RESERVATION.get() {
            return Err(Error::OutOfMemory);
        }
        let pointer = unsafe {
            match self.raw.try_allocate(CONTEXTS, bytes) {
                Some(pointer) => pointer,
                None => self.raw.allocate_slow(CONTEXTS, bytes)?,
            }
        }
        .cast::<RawContext>();
        // The sole managed allocation is initialized completely before root
        // publication. On failure above, no partial payload has been allocated.
        unsafe {
            std::ptr::copy_nonoverlapping(context.as_ptr(), pointer.as_ptr(), context.len());
        }
        Ok(RawSlice::new(pointer, context.len()))
    }
    pub fn contexts<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        contexts: &[ContextInput<'_, 'h>],
    ) -> Result<ContextRoot<'r, 'h>> {
        if contexts.is_empty() {
            return Ok(roots.empty_context());
        }
        let pointer = self
            .reserve(CONTEXTS, array_bytes::<RawContext>(contexts.len())?)?
            .cast::<RawContext>();
        for (index, context) in contexts.iter().enumerate() {
            // Inputs are root handles; decode only after reserve may collect.
            unsafe { pointer.as_ptr().add(index).write(context.raw()) };
        }
        Ok(roots.push::<ContextKind>(RawSlice::new(pointer, contexts.len())))
    }
    pub fn string<'r>(&mut self, roots: &Roots<'r, 'h>, bytes: &[u8]) -> Result<ValueRoot<'r, 'h>> {
        let bytes = self.bytes(roots, bytes)?;
        self.string_from_bytes(roots, bytes, roots.empty_context())
    }
    pub fn string_with_context<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        bytes: &[u8],
        contexts: ContextRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let bytes = self.bytes(roots, bytes)?;
        self.string_from_bytes(roots, bytes, contexts)
    }
    pub fn string_from_bytes<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        bytes: BytesRoot<'_, 'h>,
        context: ContextRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let empty = bytes.raw.read().len == 0 && context.raw.read().len == 0;
        if empty {
            if let Some(value) = &self.empty_string {
                return Ok(roots.copy(value.root()));
            }
        }
        // SAFETY: STRING has the unpadded fixed layout of RawStr.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawStr>() }>(STRING)? }.cast::<RawStr>();
        unsafe {
            pointer.as_ptr().write(RawStr {
                bytes: bytes.raw.read(),
                context: context.raw.read(),
            })
        };
        let value = roots.push::<ValueKind>(RawValue::String(pointer));
        if empty {
            self.empty_string = Some(self.persist(value));
        }
        Ok(value)
    }
    /// Shares the byte owner and context; no unrooted interior pointer crosses GC.
    /// Lazy-hash offset adjustment remains the evaluator's string operation.
    pub fn substring<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        source: ValueRoot<'_, 'h>,
        range: Range<usize>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let context = match self.view(source) {
            ValueView::String(string) => string.context_root(roots),
            _ => return Err(Error::InvalidRange),
        };
        self.substring_with_context(roots, source, range, context)
    }
    /// Creates a string header over existing bytes with a replacement context.
    /// Source and context are exact roots and are reloaded after reserve can GC.
    pub fn substring_with_context<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        source: ValueRoot<'_, 'h>,
        range: Range<usize>,
        context: ContextRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let len = match self.view(source) {
            ValueView::String(s) => s.bytes().len(),
            _ => return Err(Error::InvalidRange),
        };
        if range.start > range.end || range.end > len {
            return Err(Error::InvalidRange);
        }
        if range.is_empty() && context.raw.read().len == 0 {
            return self.string(roots, b"");
        }
        // SAFETY: STRING has the unpadded fixed layout of RawStr.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawStr>() }>(STRING)? }.cast::<RawStr>();
        unsafe {
            let RawValue::String(source) = source.raw.read() else {
                unreachable!()
            };
            let source = source.as_ref();
            // Empty slices need no byte owner; nonempty interior slices retain
            // the original allocation through the collector's interior roots.
            let bytes = if range.is_empty() {
                RawSlice::empty()
            } else {
                RawSlice::new(
                    NonNull::new_unchecked(source.bytes.ptr.as_ptr().add(range.start)),
                    range.len(),
                )
            };
            pointer.as_ptr().write(RawStr {
                bytes,
                context: context.raw.read(),
            });
        }
        Ok(roots.push::<ValueKind>(RawValue::String(pointer)))
    }
    pub fn path<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        accessor: AccessorId,
        path: &[u8],
    ) -> Result<ValueRoot<'r, 'h>> {
        let bytes = self.bytes(roots, path)?;
        // SAFETY: PATH has the unpadded fixed layout of RawPath.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawPath>() }>(PATH)? }.cast::<RawPath>();
        unsafe {
            pointer.as_ptr().write(RawPath {
                accessor,
                path: bytes.raw.read(),
                literal: None,
            })
        };
        Ok(roots.push::<ValueKind>(RawValue::Path(pointer)))
    }
    /// Associate a freshly imported path literal with permanent code metadata.
    pub fn set_path_literal(&mut self, value: ValueRoot<'_, 'h>, node: GlobalNode) {
        unsafe {
            let RawValue::Path(pointer) = value.raw.read() else {
                panic!("path literal required")
            };
            (*pointer.as_ptr()).literal = Some(node);
        }
    }
    pub fn list<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        elems: &[CellRoot<'_, 'h>],
    ) -> Result<ValueRoot<'r, 'h>> {
        if elems.is_empty() {
            if let Some(value) = &self.empty_list {
                return Ok(roots.copy(value.root()));
            }
        }
        // The first stage is rooted as a bytes owner; its heap descriptor still
        // knows the payload is a cell-edge array and traces all its children.
        let data = if elems.is_empty() {
            RawSlice::empty()
        } else {
            let pointer = self
                .reserve(CELLS, array_bytes::<CellPtr>(elems.len())?)?
                .cast::<CellPtr>();
            for (index, cell) in elems.iter().enumerate() {
                unsafe { pointer.as_ptr().add(index).write(cell.raw.read()) };
            }
            RawSlice::new(pointer.cast::<u8>(), array_bytes::<CellPtr>(elems.len())?)
        };
        let data = roots.push::<BytesKind>(data);
        // SAFETY: LIST has the unpadded fixed layout of RawList.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawList>() }>(LIST)? }.cast::<RawList>();
        unsafe {
            pointer.as_ptr().write(RawList {
                elems: RawSlice::new(data.raw.read().ptr.cast(), elems.len()),
            })
        };
        let value = roots.push::<ValueKind>(RawValue::List(pointer));
        if elems.is_empty() {
            self.empty_list = Some(self.persist(value));
        }
        Ok(value)
    }
    /// Concatenate rooted lists without separately rooting every element.
    /// Both input owners remain live through reservation; their relocated edge
    /// arrays are copied only after reservation has completed.
    pub fn concat_lists<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        left: ValueRoot<'_, 'h>,
        right: ValueRoot<'_, 'h>,
    ) -> Result<ValueRoot<'r, 'h>> {
        let (left_len, right_len) = match (self.view(left), self.view(right)) {
            (ValueView::List(left), ValueView::List(right)) => (left.len(), right.len()),
            _ => return Err(Error::InvalidRange),
        };
        if left_len == 0 {
            return Ok(roots.copy(right));
        }
        if right_len == 0 {
            return Ok(roots.copy(left));
        }
        let len = left_len
            .checked_add(right_len)
            .ok_or(Error::AllocationTooLarge)?;
        let bytes = array_bytes::<CellPtr>(len)?;
        let edges = self.reserve(CELLS, bytes)?.cast::<CellPtr>();
        unsafe {
            // Reservation may have moved both lists and their cells. Reload
            // each input from its root; no collection occurs during either copy.
            let (RawValue::List(left), RawValue::List(right)) = (left.raw.read(), right.raw.read())
            else {
                unreachable!("checked list operands")
            };
            std::ptr::copy_nonoverlapping(
                left.as_ref().elems.ptr.as_ptr(),
                edges.as_ptr(),
                left_len,
            );
            std::ptr::copy_nonoverlapping(
                right.as_ref().elems.ptr.as_ptr(),
                edges.as_ptr().add(left_len),
                right_len,
            );
        }
        // The CELLS descriptor traces the copied children if the header's
        // allocation collects. A single owner root protects the entire array.
        let data = roots.push::<BytesKind>(RawSlice::new(edges.cast(), bytes));
        // SAFETY: LIST has the unpadded fixed layout of RawList.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawList>() }>(LIST)? }.cast::<RawList>();
        unsafe {
            pointer.as_ptr().write(RawList {
                elems: RawSlice::new(data.raw.read().ptr.cast(), len),
            });
        }
        Ok(roots.push::<ValueKind>(RawValue::List(pointer)))
    }

    /// A single-argument lambda frame, using a fixed-size traced allocation.
    #[inline]
    pub fn env_one<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        up: EnvRoot<'_, 'h>,
        value: CellRoot<'_, 'h>,
    ) -> Result<EnvRoot<'r, 'h>> {
        // SAFETY: ENV_ONE is exactly RawEnvOne, without a variable-size prefix.
        let pointer = unsafe { self.reserve_fixed::<{ size_of::<RawEnvOne>() }>(ENV_ONE)? }
            .cast::<RawEnvOne>();
        // Reservation can collect; reload both roots only afterwards. The
        // header and sole slot are initialized before publishing the root.
        unsafe {
            pointer.as_ptr().write(RawEnvOne {
                header: RawEnv {
                    up: Some(up.raw.read()),
                    len: 1,
                },
                value: value.raw.read(),
            });
        }
        Ok(roots.push::<EnvKind>(pointer.cast()))
    }

    /// Construct a single-argument frame directly from its rooted lambda.
    /// The function owns the captured environment across reservation and GC.
    #[inline]
    pub fn env_one_from_lambda<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        function: ValueRoot<'_, 'h>,
        value: CellRoot<'_, 'h>,
    ) -> Result<EnvRoot<'r, 'h>> {
        if !matches!(function.raw.read(), RawValue::Lambda { .. }) {
            return Err(Error::InvalidRange);
        }
        // SAFETY: ENV_ONE is exactly RawEnvOne. Validate the source before
        // reserving any uninitialized object; reservation can only relocate
        // the rooted lambda, never change its variant or invoke user code.
        let pointer = unsafe { self.reserve_fixed::<{ size_of::<RawEnvOne>() }>(ENV_ONE)? }
            .cast::<RawEnvOne>();
        let RawValue::Lambda { env, .. } = function.raw.read() else {
            unreachable!("reservation preserves the rooted lambda")
        };
        unsafe {
            pointer.as_ptr().write(RawEnvOne {
                header: RawEnv {
                    up: Some(env),
                    len: 1,
                },
                value: value.raw.read(),
            });
        }
        Ok(roots.push::<EnvKind>(pointer.cast()))
    }

    pub fn env<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        up: Option<EnvRoot<'_, 'h>>,
        elems: &[CellRoot<'_, 'h>],
    ) -> Result<EnvRoot<'r, 'h>> {
        let len = u32::try_from(elems.len()).map_err(|_| Error::AllocationTooLarge)?;
        let layout = RawEnv::layout(elems.len()).map_err(|_| Error::AllocationTooLarge)?;
        let pointer = self.reserve(ENV, layout.size())?.cast::<RawEnv>();
        unsafe {
            pointer.as_ptr().write(RawEnv {
                up: up.map(|root| root.raw.read()),
                len,
            });
            let slots = pointer
                .as_ptr()
                .cast::<u8>()
                .add(ENV_SLOTS_OFFSET)
                .cast::<CellPtr>();
            for (index, cell) in elems.iter().enumerate() {
                slots.add(index).write(cell.raw.read());
            }
        }
        Ok(roots.push::<EnvKind>(pointer))
    }
    /// Construct an environment whose slots initially alias one cell. The
    /// source roots are reloaded after allocation may collect; no intermediate
    /// vector or repeated root reads are needed for the placeholder slots.
    pub fn env_filled<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        up: Option<EnvRoot<'_, 'h>>,
        len: usize,
        value: CellRoot<'_, 'h>,
    ) -> Result<EnvRoot<'r, 'h>> {
        let header_len = u32::try_from(len).map_err(|_| Error::AllocationTooLarge)?;
        let layout = RawEnv::layout(len).map_err(|_| Error::AllocationTooLarge)?;
        let pointer = self.reserve(ENV, layout.size())?.cast::<RawEnv>();
        // SAFETY: the allocation has room for exactly len initialized slots.
        // Read both inputs only after reserve, then initialize without another
        // collecting operation before publishing the completed environment.
        unsafe {
            let value = value.raw.read();
            pointer.as_ptr().write(RawEnv {
                up: up.map(|root| root.raw.read()),
                len: header_len,
            });
            let slots = pointer
                .as_ptr()
                .cast::<u8>()
                .add(ENV_SLOTS_OFFSET)
                .cast::<CellPtr>();
            for index in 0..len {
                slots.add(index).write(value);
            }
        }
        Ok(roots.push::<EnvKind>(pointer))
    }
    pub fn env_view<'v>(&'v self, root: EnvRoot<'_, 'h>) -> EnvView<'v, 'h> {
        EnvView::new(root.raw.read())
    }
    pub fn env_set(&mut self, env: EnvRoot<'_, 'h>, index: usize, cell: CellRoot<'_, 'h>) {
        self.raw.write_barrier(env.raw.read().cast());
        unsafe {
            let pointer = env.raw.read();
            assert!(
                index < pointer.as_ref().len as usize,
                "environment slot out of bounds"
            );
            pointer
                .as_ptr()
                .cast::<u8>()
                .add(ENV_SLOTS_OFFSET)
                .cast::<CellPtr>()
                .add(index)
                .write(cell.raw.read());
        }
    }
    /// Set a lexical parent after registering a cyclic bootstrap graph.
    pub fn env_set_up(&mut self, env: EnvRoot<'_, 'h>, up: Option<EnvRoot<'_, 'h>>) {
        self.raw.write_barrier(env.raw.read().cast());
        unsafe {
            (*env.raw.read().as_ptr()).up = up.map(|root| root.raw.read());
        }
    }
    pub fn thunk<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        node: GlobalNode,
    ) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::Thunk {
            env: env.raw.read(),
            node,
        })
    }
    pub fn lambda<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        node: GlobalNode,
    ) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::Lambda {
            env: env.raw.read(),
            node,
        })
    }
    /// Allocate an application cell without an intermediate value root.
    #[inline]
    pub fn app_cell<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
    ) -> Result<CellRoot<'r, 'h>> {
        // SAFETY: CELL describes the nonzero, unpadded RawCell layout.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawCell>() }>(CELL)? }.cast::<RawCell>();
        // Publish an initialized cell before reserving the application. That
        // reservation may collect; a blackhole contains no managed edges.
        unsafe { pointer.as_ptr().write(RawCell::new(RawValue::Blackhole)) };
        let cell = roots.push::<CellKind>(pointer);
        // SAFETY: APP describes the nonzero, unpadded RawApp layout.
        let app = unsafe { self.reserve_fixed::<{ size_of::<RawApp>() }>(APP)? }.cast::<RawApp>();
        // The cell may have been promoted while reserving its application.
        self.raw.write_barrier(cell.raw.read().cast());
        // Reload all three roots after the collecting reservation, then publish
        // the application through the cell without another collecting call.
        unsafe {
            app.as_ptr().write(RawApp {
                fun: fun.raw.read(),
                arg: arg.raw.read(),
            });
            cell.raw
                .read()
                .as_ptr()
                .write(RawCell::new(RawValue::App(app)));
        }
        Ok(cell)
    }

    pub fn app<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
        partial: bool,
    ) -> Result<ValueRoot<'r, 'h>> {
        // SAFETY: APP describes RawApp, whose Rust size includes alignment.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawApp>() }>(APP)? }.cast::<RawApp>();
        unsafe {
            pointer.as_ptr().write(RawApp {
                fun: fun.raw.read(),
                arg: arg.raw.read(),
            })
        };
        Ok(roots.push::<ValueKind>(if partial {
            RawValue::PrimOpApp(pointer)
        } else {
            RawValue::App(pointer)
        }))
    }
    pub fn failed<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        error: crate::Error,
    ) -> Result<ValueRoot<'r, 'h>> {
        // SAFETY: ERROR has the unpadded fixed layout of RawError.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawError>() }>(ERROR)? }.cast::<RawError>();
        unsafe { pointer.as_ptr().write(RawError { error }) };
        Ok(roots.push::<ValueKind>(RawValue::Failed(pointer)))
    }
    pub fn primop<'r>(&self, roots: &Roots<'r, 'h>, id: usize) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::PrimOp(id))
    }
    fn decode<'v>(&'v self, value: RawValue) -> ValueView<'v, 'h> {
        match value {
            RawValue::Int(v) => ValueView::Int(v),
            RawValue::Float(v) => ValueView::Float(v),
            RawValue::Bool(v) => ValueView::Bool(v),
            RawValue::Null => ValueView::Null,
            RawValue::Blackhole => ValueView::Blackhole,
            RawValue::String(p) => ValueView::String(StringView::new(p)),
            RawValue::Path(p) => ValueView::Path(PathView::new(p)),
            RawValue::List(p) => ValueView::List(ListView::new(p)),
            RawValue::Attrs(p) => ValueView::Attrs(BindingsView::new(p)),
            RawValue::Lambda { env, node } => ValueView::Lambda {
                env: EnvView::new(env),
                node,
            },
            RawValue::Thunk { env, node } => ValueView::Thunk {
                env: EnvView::new(env),
                node,
            },
            RawValue::App(p) => ValueView::App(AppView::new(p)),
            RawValue::PrimOpApp(p) => ValueView::PrimOpApp(AppView::new(p)),
            RawValue::PrimOp(id) => ValueView::PrimOp(id),
            RawValue::Failed(p) => ValueView::Failed(unsafe { &p.as_ref().error }),
        }
    }
}

fn array_bytes<T>(len: usize) -> Result<usize> {
    len.checked_mul(size_of::<T>())
        .ok_or(Error::AllocationTooLarge)
}
unsafe fn slice<'v, T>(value: RawSlice<T>) -> &'v [T] {
    unsafe { std::slice::from_raw_parts(value.ptr.as_ptr(), value.len) }
}

#[derive(Clone, Copy)]
pub(crate) enum ContextInput<'r, 'h> {
    LazyHash {
        offset: u32,
        tree: u32,
    },
    LazyPath(u32),
    Opaque(BytesRoot<'r, 'h>),
    DrvDeep(BytesRoot<'r, 'h>),
    Path(BytesRoot<'r, 'h>),
    Built {
        drv: BytesRoot<'r, 'h>,
        output: BytesRoot<'r, 'h>,
    },
}
impl ContextInput<'_, '_> {
    /// The raw payload is only current until the next collection.
    fn raw(self) -> RawContext {
        match self {
            Self::LazyHash { offset, tree } => RawContext::LazyHash { offset, tree },
            Self::LazyPath(id) => RawContext::LazyPath(id),
            Self::Opaque(v) => RawContext::Opaque(v.raw.read()),
            Self::DrvDeep(v) => RawContext::DrvDeep(v.raw.read()),
            Self::Path(v) => RawContext::Path(v.raw.read()),
            Self::Built { drv, output } => RawContext::Built {
                drv: drv.raw.read(),
                output: output.raw.read(),
            },
        }
    }
}

macro_rules! view {
    ($name:ident, $raw:ty) => {
        #[derive(Clone, Copy)]
        pub(crate) struct $name<'v, 'h> {
            pointer: NonNull<$raw>,
            borrow: PhantomData<&'v Heap<'h>>,
        }
        impl<'v, 'h> $name<'v, 'h> {
            fn new(pointer: NonNull<$raw>) -> Self {
                Self {
                    pointer,
                    borrow: PhantomData,
                }
            }
        }
    };
}
view!(StringView, RawStr);
view!(PathView, RawPath);
view!(ListView, RawList);
view!(EnvView, RawEnv);
view!(BindingsView, RawBindings);
view!(CellView, RawCell);
view!(AppView, RawApp);

#[derive(Clone, Copy)]
// Match RawValue's variant order so safe decoding can retain its discriminant.
// decode still matches variants explicitly; correctness assumes no shared layout.
pub(crate) enum ValueView<'v, 'h> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String(StringView<'v, 'h>),
    Path(PathView<'v, 'h>),
    Attrs(BindingsView<'v, 'h>),
    List(ListView<'v, 'h>),
    Lambda {
        env: EnvView<'v, 'h>,
        node: GlobalNode,
    },
    PrimOp(usize),
    PrimOpApp(AppView<'v, 'h>),
    Thunk {
        env: EnvView<'v, 'h>,
        node: GlobalNode,
    },
    App(AppView<'v, 'h>),
    Blackhole,
    Failed(&'v crate::Error),
}
impl<'v, 'h> StringView<'v, 'h> {
    /// Inspect only context tags; ordinary path/derivation contexts do not
    /// affect string comparison and need no borrowed payload decoding.
    #[inline]
    pub(crate) fn has_lazy_hash(self) -> bool {
        unsafe { slice(self.pointer.as_ref().context) }
            .iter()
            .any(|context| matches!(context, RawContext::LazyHash { .. }))
    }
    pub fn bytes(self) -> &'v [u8] {
        unsafe { slice(self.pointer.as_ref().bytes) }
    }
    pub fn bytes_root<'r>(self, roots: &Roots<'r, 'h>) -> BytesRoot<'r, 'h> {
        roots.push::<BytesKind>(unsafe { self.pointer.as_ref().bytes })
    }
    pub fn context_root<'r>(self, roots: &Roots<'r, 'h>) -> ContextRoot<'r, 'h> {
        roots.push::<ContextKind>(unsafe { self.pointer.as_ref().context })
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::String(self.pointer))
    }
    pub fn contexts(self) -> impl Iterator<Item = ContextView<'v>> {
        unsafe { slice(self.pointer.as_ref().context) }
            .iter()
            .map(|c| unsafe { ContextView::from_raw(*c) })
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum ContextView<'v> {
    LazyHash { offset: u32, tree: u32 },
    LazyPath(u32),
    Opaque(&'v [u8]),
    DrvDeep(&'v [u8]),
    Path(&'v [u8]),
    Built { drv: &'v [u8], output: &'v [u8] },
}
impl<'v> ContextView<'v> {
    unsafe fn from_raw(value: RawContext) -> Self {
        unsafe {
            match value {
                RawContext::LazyHash { offset, tree } => Self::LazyHash { offset, tree },
                RawContext::LazyPath(id) => Self::LazyPath(id),
                RawContext::Opaque(v) => Self::Opaque(slice(v)),
                RawContext::DrvDeep(v) => Self::DrvDeep(slice(v)),
                RawContext::Path(v) => Self::Path(slice(v)),
                RawContext::Built { drv, output } => Self::Built {
                    drv: slice(drv),
                    output: slice(output),
                },
            }
        }
    }
}
impl<'v, 'h> CellView<'v, 'h> {
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> CellRoot<'r, 'h> {
        roots.push::<CellKind>(self.pointer)
    }
}
impl<'v, 'h> PathView<'v, 'h> {
    pub fn literal(self) -> Option<GlobalNode> {
        unsafe { self.pointer.as_ref().literal }
    }
    pub fn accessor(self) -> AccessorId {
        unsafe { self.pointer.as_ref().accessor }
    }
    pub fn bytes(self) -> &'v [u8] {
        unsafe { slice(self.pointer.as_ref().path) }
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::Path(self.pointer))
    }
}
impl<'v, 'h> ListView<'v, 'h> {
    pub fn len(self) -> usize {
        unsafe { self.pointer.as_ref().elems.len }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn get(self, index: usize) -> Option<CellView<'v, 'h>> {
        unsafe { slice(self.pointer.as_ref().elems) }
            .get(index)
            .copied()
            .map(CellView::new)
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(RawValue::List(self.pointer))
    }
}
impl<'v, 'h> EnvView<'v, 'h> {
    pub fn len(self) -> usize {
        unsafe { self.pointer.as_ref().len as usize }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn up(self) -> Option<Self> {
        unsafe { self.pointer.as_ref().up }.map(Self::new)
    }
    pub fn get(self, index: usize) -> Option<CellView<'v, 'h>> {
        if index >= self.len() {
            return None;
        }
        Some(CellView::new(unsafe {
            self.pointer
                .as_ptr()
                .cast::<u8>()
                .add(ENV_SLOTS_OFFSET)
                .cast::<CellPtr>()
                .add(index)
                .read()
        }))
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> EnvRoot<'r, 'h> {
        roots.push::<EnvKind>(self.pointer)
    }
}
impl<'v, 'h> AppView<'v, 'h> {
    pub fn fun(self) -> CellView<'v, 'h> {
        CellView::new(unsafe { self.pointer.as_ref().fun })
    }
    pub fn arg(self) -> CellView<'v, 'h> {
        CellView::new(unsafe { self.pointer.as_ref().arg })
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>, partial: bool) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(if partial {
            RawValue::PrimOpApp(self.pointer)
        } else {
            RawValue::App(self.pointer)
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RootedAttr<'r, 'h> {
    pub name: Symbol,
    pub pos: SrcPos,
    pub value: CellRoot<'r, 'h>,
}
/// Immutable compilation metadata, independent of any managed heap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ThunkAttr {
    pub name: Symbol,
    pub pos: SrcPos,
    pub node: GlobalNode,
}

/// A checked, bounded plan. Its fields cannot be changed after validation.
/// A batch constructs at most 64 cells and one flat binding allocation.
pub(crate) struct FlatThunkPlan<'a> {
    attrs: &'a [ThunkAttr],
    pos: SrcPos,
    binding_bytes: usize,
}
impl<'a> FlatThunkPlan<'a> {
    pub fn new(pos: SrcPos, attrs: &'a [ThunkAttr]) -> Result<Self> {
        if attrs.is_empty()
            || attrs.len() > crate::bindings::FLAT_MAX
            || !attrs.windows(2).all(|pair| pair[0].name < pair[1].name)
        {
            return Err(Error::InvalidRange);
        }
        let binding_bytes = RawBindings::flat_layout(attrs.len())
            .map_err(|_| Error::AllocationTooLarge)?
            .size();
        Ok(Self {
            attrs,
            pos,
            binding_bytes,
        })
    }
}

/// Heap-independent sources for a nonrecursive, statically named flat set.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LazyAttrSource {
    Thunk(GlobalNode),
    Variable {
        level: u32,
        displ: u32,
        fallback: GlobalNode,
    },
    Literal(u32),
    EmptyList,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct LazyAttr {
    pub name: Symbol,
    pub pos: SrcPos,
    pub source: LazyAttrSource,
}
pub(crate) struct FlatLazyPlan<'a> {
    attrs: &'a [LazyAttr],
    pos: SrcPos,
    binding_bytes: usize,
}
impl<'a> FlatLazyPlan<'a> {
    pub fn new(pos: SrcPos, attrs: &'a [LazyAttr]) -> Result<Self> {
        if attrs.is_empty()
            || attrs.len() > crate::bindings::FLAT_MAX
            || !attrs.windows(2).all(|pair| pair[0].name < pair[1].name)
        {
            return Err(Error::InvalidRange);
        }
        Ok(Self {
            attrs,
            pos,
            binding_bytes: RawBindings::flat_layout(attrs.len())
                .map_err(|_| Error::AllocationTooLarge)?
                .size(),
        })
    }
}

pub(crate) enum NodeContents<'r, 'h> {
    Leaf(Vec<RootedAttr<'r, 'h>>),
    Inner {
        children: Vec<NodeRoot<'r, 'h>>,
        firsts: Vec<Symbol>,
    },
}
pub(crate) enum BindingsContents<'r, 'h> {
    Flat {
        attrs: Vec<RootedAttr<'r, 'h>>,
        pos: SrcPos,
    },
    Tree {
        root: NodeRoot<'r, 'h>,
        len: usize,
        pos: SrcPos,
    },
}

impl<'h> Heap<'h> {
    pub fn bindings_value<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        set: BindingsRoot<'_, 'h>,
    ) -> ValueRoot<'r, 'h> {
        let view = self.bindings_view(set);
        let pointer = if view.is_empty() && view.pos() == SrcPos::NONE {
            let mut canonical = self.empty_attrs.borrow_mut();
            let canonical = canonical.get_or_insert_with(|| self.persist(set));
            canonical.root().raw.read()
        } else {
            set.raw.read()
        };
        roots.push::<ValueKind>(RawValue::Attrs(pointer))
    }
    pub fn bindings_view<'v>(&'v self, set: BindingsRoot<'_, 'h>) -> BindingsView<'v, 'h> {
        BindingsView::new(set.raw.read())
    }
    pub fn bindings_snapshot<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        set: BindingsRoot<'_, 'h>,
    ) -> BindingsContents<'r, 'h> {
        let view = self.bindings_view(set);
        if view.len() <= crate::bindings::FLAT_MAX {
            BindingsContents::Flat {
                attrs: view.iter().map(|a| a.root(roots)).collect(),
                pos: view.pos(),
            }
        } else {
            let pointer = unsafe { set.raw.read().cast::<RawTreeBindings>().as_ref().root };
            BindingsContents::Tree {
                root: roots.push::<NodeKind>(pointer),
                len: view.len(),
                pos: view.pos(),
            }
        }
    }
    pub fn node_snapshot<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        node: NodeRoot<'_, 'h>,
    ) -> NodeContents<'r, 'h> {
        unsafe {
            match *node.raw.read().as_ref() {
                RawNode::Leaf(attrs) => NodeContents::Leaf(
                    slice(attrs)
                        .iter()
                        .map(|a| RootedAttr {
                            name: a.name,
                            pos: a.pos,
                            value: roots.push::<CellKind>(a.value),
                        })
                        .collect(),
                ),
                RawNode::Inner { children, firsts } => NodeContents::Inner {
                    children: slice(children)
                        .iter()
                        .map(|p| roots.push::<NodeKind>(*p))
                        .collect(),
                    firsts: slice(firsts).to_vec(),
                },
            }
        }
    }
    /// Build a bounded set of suspended expressions with one safepoint and no
    /// temporary cell roots. Every thunk captures the same rooted environment.
    pub fn flat_thunk_attrs<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        plan: &FlatThunkPlan<'_>,
    ) -> Result<BindingsRoot<'r, 'h>> {
        // The plan validates payload geometry once. Physical extents belong to
        // the backend; account for both kinds without visiting individual attrs.
        let cells = self
            .raw
            .allocation_extent(CELL, size_of::<RawCell>())?
            .checked_mul(plan.attrs.len())
            .ok_or(Error::AllocationTooLarge)?;
        let pending = cells
            .checked_add(
                self.raw
                    .allocation_extent(FLAT_BINDINGS, plan.binding_bytes)?,
            )
            .ok_or(Error::AllocationTooLarge)?;
        if self.raw.collection_due(pending) {
            self.collect_minor()?;
        }
        // No collecting allocations or callbacks below. Each cell is fully
        // initialized before another allocation can fail. Ordinary scratch
        // storage holds at most 64 entries and cannot spill to the allocator.
        let env = env.raw.read();
        let mut attrs = SmallVec::<[RawAttr; 64]>::new();
        for attr in plan.attrs {
            #[cfg(test)]
            flat_thunk_allocation_checkpoint()?;
            // SAFETY: CELL is a known fixed RawCell descriptor. Both allocation
            // paths bypass collection, keeping env and prior cells stationary.
            let pointer = unsafe {
                match self
                    .raw
                    .try_allocate_known_fixed::<{ size_of::<RawCell>() }>(CELL)
                {
                    Some(pointer) => pointer,
                    None => self.raw.allocate_slow(CELL, size_of::<RawCell>())?,
                }
            }
            .cast::<RawCell>();
            unsafe {
                pointer.as_ptr().write(RawCell::new(RawValue::Thunk {
                    env,
                    node: attr.node,
                }))
            };
            attrs.push(RawAttr {
                name: attr.name,
                pos: attr.pos,
                value: pointer,
            });
        }
        #[cfg(test)]
        flat_thunk_allocation_checkpoint()?;
        let header = unsafe {
            match self.raw.try_allocate(FLAT_BINDINGS, plan.binding_bytes) {
                Some(pointer) => pointer,
                None => self.raw.allocate_slow(FLAT_BINDINGS, plan.binding_bytes)?,
            }
        };
        // No fallible operation remains between header allocation and complete
        // initialization. Failed earlier batches contain only valid cells.
        unsafe {
            header.cast::<RawBindings>().as_ptr().write(RawBindings {
                pos: plan.pos,
                len: plan.attrs.len() as u32,
            });
            std::ptr::copy_nonoverlapping(
                attrs.as_ptr(),
                header.as_ptr().add(FLAT_ATTRS_OFFSET).cast(),
                attrs.len(),
            );
        }
        Ok(roots.push::<BindingsKind>(header.cast()))
    }

    /// Build mixed lazy attributes without registering each shared cell as a
    /// temporary root. The environment and persistent leases own every input.
    pub fn flat_lazy_attrs<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        plan: &FlatLazyPlan<'_>,
        literals: &[Option<Lease<'h, CellKind>>],
        empty_list: CellRoot<'_, 'h>,
        uninit: CellRoot<'_, 'h>,
    ) -> Result<BindingsRoot<'r, 'h>> {
        fn variable<'v, 'h>(
            mut env: EnvView<'v, 'h>,
            level: u32,
            displ: u32,
        ) -> Result<CellView<'v, 'h>> {
            for _ in 0..level {
                env = env.up().ok_or(Error::InvalidRange)?;
            }
            env.get(displ as usize).ok_or(Error::InvalidRange)
        }
        fn prepare<'h>(
            env: EnvView<'_, 'h>,
            plan: &FlatLazyPlan<'_>,
            literals: &[Option<Lease<'h, CellKind>>],
            empty: CellPtr,
            missing: CellPtr,
            attrs: &mut SmallVec<[RawAttr; 64]>,
        ) -> Result<u64> {
            debug_assert!(attrs.is_empty());
            let mut thunks = 0u64;
            for (index, attr) in plan.attrs.iter().enumerate() {
                let value = match attr.source {
                    LazyAttrSource::Thunk(_) => {
                        thunks |= 1u64 << index;
                        missing
                    }
                    LazyAttrSource::Variable { level, displ, .. } => {
                        let value = variable(env, level, displ)?.pointer;
                        if value == missing {
                            thunks |= 1u64 << index;
                        }
                        value
                    }
                    LazyAttrSource::Literal(index) => {
                        let literal = literals
                            .get(index as usize)
                            .and_then(Option::as_ref)
                            .ok_or(Error::InvalidRange)?;
                        literal.root().raw.read()
                    }
                    LazyAttrSource::EmptyList => empty,
                };
                attrs.push(RawAttr {
                    name: attr.name,
                    pos: attr.pos,
                    value,
                });
            }
            Ok(thunks)
        }
        let mut attrs = SmallVec::<[RawAttr; 64]>::new();
        let mut thunks = prepare(
            self.env_view(env),
            plan,
            literals,
            empty_list.raw.read(),
            uninit.raw.read(),
            &mut attrs,
        )?;
        let pending = (thunks.count_ones() as usize)
            .checked_mul(size_of::<RawCell>())
            .ok_or(Error::AllocationTooLarge)?
            .checked_add(
                self.raw
                    .allocation_extent(FLAT_BINDINGS, plan.binding_bytes)?,
            )
            .ok_or(Error::AllocationTooLarge)?;
        if self.raw.collection_due(pending) {
            // Scratch is not a root. Discard its pointers before collection and
            // rebuild from the rooted environment and persistent leases after.
            attrs.clear();
            self.collect_minor()?;
            thunks = prepare(
                self.env_view(env),
                plan,
                literals,
                empty_list.raw.read(),
                uninit.raw.read(),
                &mut attrs,
            )
            .expect("collection preserves validated sources");
        }
        // Without GC, all source pointers remain current and are reused without
        // another environment walk. Raw refill below cannot collect. Each
        // placeholder is a valid sentinel pointer until its thunk is initialized.
        let environment = env.raw.read();
        while thunks != 0 {
            let index = thunks.trailing_zeros() as usize;
            thunks &= thunks - 1;
            let node = match plan.attrs[index].source {
                LazyAttrSource::Thunk(node) => node,
                LazyAttrSource::Variable { fallback, .. } => fallback,
                _ => unreachable!("only suspended sources enter the thunk mask"),
            };
            #[cfg(test)]
            flat_thunk_allocation_checkpoint()?;
            let pointer = unsafe {
                match self
                    .raw
                    .try_allocate_known_fixed::<{ size_of::<RawCell>() }>(CELL)
                {
                    Some(pointer) => pointer,
                    None => self.raw.allocate_slow(CELL, size_of::<RawCell>())?,
                }
            }
            .cast::<RawCell>();
            unsafe {
                pointer.as_ptr().write(RawCell::new(RawValue::Thunk {
                    env: environment,
                    node,
                }))
            };
            attrs[index].value = pointer;
        }
        #[cfg(test)]
        flat_thunk_allocation_checkpoint()?;
        let pointer = unsafe {
            match self.raw.try_allocate(FLAT_BINDINGS, plan.binding_bytes) {
                Some(pointer) => pointer,
                None => self.raw.allocate_slow(FLAT_BINDINGS, plan.binding_bytes)?,
            }
        };
        unsafe {
            pointer.cast::<RawBindings>().as_ptr().write(RawBindings {
                pos: plan.pos,
                len: attrs.len() as u32,
            });
            std::ptr::copy_nonoverlapping(
                attrs.as_ptr(),
                pointer.as_ptr().add(FLAT_ATTRS_OFFSET).cast(),
                attrs.len(),
            );
        }
        Ok(roots.push::<BindingsKind>(pointer.cast()))
    }

    /// Copy only names whose persistent string cells have not been cached yet.
    /// The returned symbols carry no heap references, so callers may allocate
    /// their cells after this bounded view has ended.
    pub fn missing_attr_names(
        &self,
        input: BindingsRoot<'_, 'h>,
        names: &[Option<Lease<'h, CellKind>>],
    ) -> Vec<Symbol> {
        let mut source = BindingsLeafCursor::new(self.bindings_view(input));
        let mut missing = Vec::new();
        while !source.leaf.is_empty() {
            for attr in source.leaf {
                if names
                    .get(attr.name.0 as usize)
                    .and_then(Option::as_ref)
                    .is_none()
                {
                    missing.push(attr.name);
                }
            }
            source.consume(source.leaf.len());
        }
        missing
    }

    /// Build lazy `function name value` applications with one collecting boundary.
    /// Name cells are persistent leases prepared by the caller; the function is
    /// never forced, including for empty sets. Output positions are discarded.
    pub fn map_attrs_lazy<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        input: BindingsRoot<'_, 'h>,
        function: CellRoot<'_, 'h>,
        names: &[Option<Lease<'h, CellKind>>],
    ) -> Result<BindingsRoot<'r, 'h>> {
        const FANOUT: usize = 64;
        let len = self.bindings_view(input).len();
        if len == 0 {
            return self.alloc_flat_bindings(roots, SrcPos::NONE, &[]);
        }
        let header_len = u32::try_from(len).map_err(|_| Error::AllocationTooLarge)?;
        fn groups(len: usize) -> impl Iterator<Item = usize> {
            let count = len.div_ceil(FANOUT);
            let base = len / count;
            let extra = len % count;
            (0..count).map(move |index| base + usize::from(index < extra))
        }
        let flat = len <= crate::bindings::FLAT_MAX;
        let leaf_count = len.div_ceil(FANOUT);
        let mut pending = 0usize;
        let mut account = |kind, bytes, count: usize| -> Result<()> {
            let extent = self.raw.allocation_extent(kind, bytes)?;
            pending = pending
                .checked_add(extent.checked_mul(count).ok_or(Error::AllocationTooLarge)?)
                .ok_or(Error::AllocationTooLarge)?;
            Ok(())
        };
        let applications = len.checked_mul(2).ok_or(Error::AllocationTooLarge)?;
        account(APP, size_of::<RawApp>(), applications)?;
        account(CELL, size_of::<RawCell>(), applications)?;
        if flat {
            account(
                FLAT_BINDINGS,
                RawBindings::flat_layout(len)
                    .map_err(|_| Error::AllocationTooLarge)?
                    .size(),
                1,
            )?;
        } else {
            for count in groups(len) {
                account(ATTRS, array_bytes::<RawAttr>(count)?, 1)?;
                account(NODE, size_of::<RawNode>(), 1)?;
            }
            let mut count = leaf_count;
            while count > 1 {
                for children in groups(count) {
                    account(SYMBOLS, array_bytes::<Symbol>(children)?, 1)?;
                    account(NODES, array_bytes::<NonNull<RawNode>>(children)?, 1)?;
                    account(NODE, size_of::<RawNode>(), 1)?;
                }
                count = count.div_ceil(FANOUT);
            }
            account(TREE_BINDINGS, size_of::<RawTreeBindings>(), 1)?;
        }
        let mut nodes = Vec::<NonNull<RawNode>>::new();
        let mut parents = Vec::<NonNull<RawNode>>::new();
        if !flat {
            nodes
                .try_reserve_exact(leaf_count)
                .map_err(|_| Error::OutOfMemory)?;
            parents
                .try_reserve_exact(leaf_count.div_ceil(FANOUT))
                .map_err(|_| Error::OutOfMemory)?;
        }
        if self.raw.collection_due(pending) {
            self.collect_minor()?;
        }
        // No collection or arbitrary callbacks below. Every allocated payload is
        // initialized before another allocation can fail. A failed batch leaves
        // only valid, unreachable objects, safe to trace or destroy afterwards.
        #[inline]
        unsafe fn allocate(raw: &mut RawHeap, kind: usize, bytes: usize) -> Result<NonNull<u8>> {
            #[cfg(test)]
            map_attrs_allocation_checkpoint()?;
            if let Some(pointer) = unsafe { raw.try_allocate(kind, bytes) } {
                return Ok(pointer);
            }
            unsafe { raw.allocate_slow(kind, bytes) }
        }
        // Reload every input after the sole collecting boundary. Views and raw
        // pointers stay private to this construction, whose allocator cannot GC.
        let source: BindingsView<'_, 'h> = BindingsView::new(input.raw.read());
        let mut source = BindingsLeafCursor::new(source);
        let function = function.raw.read();
        let mut attrs = SmallVec::<[RawAttr; FANOUT]>::new();
        let mut name_cells = SmallVec::<[CellPtr; FANOUT]>::new();
        for count in groups(len) {
            attrs.clear();
            name_cells.clear();
            while attrs.len() < count {
                let take = (count - attrs.len()).min(source.leaf.len());
                assert!(take != 0, "validated attribute count");
                for attr in &source.leaf[..take] {
                    // Validate while filling ordinary scratch storage, before
                    // reserving this leaf's managed payload. Earlier apps and
                    // leaves are fully initialized even if a name is missing.
                    let name = names
                        .get(attr.name.0 as usize)
                        .and_then(Option::as_ref)
                        .ok_or(Error::InvalidRange)?;
                    name_cells.push(unsafe { name.raw.read() });
                    attrs.push(RawAttr {
                        name: attr.name,
                        pos: SrcPos::NONE,
                        value: attr.value,
                    });
                }
                source.consume(take);
            }
            // At most128 objects of each kind. The pair reservation commits
            // neither run until BOTH spaces fit, including cold refill failure.
            // Cross-edges are initialized together before any further fallible
            // operation; no temporary Null-cell initialization is necessary.
            #[cfg(test)]
            map_attrs_allocation_checkpoint()?;
            let (cells, apps) = unsafe {
                self.raw
                    .allocate_known_fixed_pair::<{ size_of::<RawCell>() }, { size_of::<RawApp>() }>(
                        CELL,
                        2 * count,
                        APP,
                        2 * count,
                    )?
            };
            let cells = cells.cast::<RawCell>();
            let apps = apps.cast::<RawApp>();
            // Every scratch entry and name cell was validated before either
            // run was reserved. This bounded loop cannot allocate, fail or call
            // user code, and initializes every app and cell exactly once.
            for (index, (attr, name)) in attrs.iter_mut().zip(&name_cells).enumerate() {
                unsafe {
                    let first_cell = cells.add(2 * index);
                    let second_cell = cells.add(2 * index + 1);
                    let first_app = apps.add(2 * index);
                    let second_app = apps.add(2 * index + 1);
                    first_app.as_ptr().write(RawApp {
                        fun: function,
                        arg: *name,
                    });
                    second_app.as_ptr().write(RawApp {
                        fun: first_cell,
                        arg: attr.value,
                    });
                    first_cell
                        .as_ptr()
                        .write(RawCell::new(RawValue::App(first_app)));
                    second_cell
                        .as_ptr()
                        .write(RawCell::new(RawValue::App(second_app)));
                    attr.value = second_cell;
                }
            }
            if flat {
                let layout = RawBindings::flat_layout(len).expect("validated flat layout");
                let header = unsafe { allocate(&mut self.raw, FLAT_BINDINGS, layout.size())? };
                unsafe {
                    header.cast::<RawBindings>().as_ptr().write(RawBindings {
                        pos: SrcPos::NONE,
                        len: header_len,
                    });
                    std::ptr::copy_nonoverlapping(
                        attrs.as_ptr(),
                        header.as_ptr().add(FLAT_ATTRS_OFFSET).cast(),
                        len,
                    );
                }
                return Ok(roots.push::<BindingsKind>(header.cast()));
            }
            let payload =
                unsafe { allocate(&mut self.raw, ATTRS, array_bytes::<RawAttr>(count)?)? }
                    .cast::<RawAttr>();
            unsafe { std::ptr::copy_nonoverlapping(attrs.as_ptr(), payload.as_ptr(), count) };
            let node =
                unsafe { allocate(&mut self.raw, NODE, size_of::<RawNode>())? }.cast::<RawNode>();
            unsafe {
                node.as_ptr()
                    .write(RawNode::Leaf(RawSlice::new(payload, count)))
            };
            nodes.push(node);
        }
        while nodes.len() > 1 {
            parents.clear();
            let mut offset = 0;
            for count in groups(nodes.len()) {
                let children = &nodes[offset..offset + count];
                offset += count;
                let firsts =
                    unsafe { allocate(&mut self.raw, SYMBOLS, array_bytes::<Symbol>(count)?)? }
                        .cast::<Symbol>();
                for (index, child) in children.iter().enumerate() {
                    unsafe { firsts.as_ptr().add(index).write(first_symbol(*child)) };
                }
                let edges = unsafe {
                    allocate(
                        &mut self.raw,
                        NODES,
                        array_bytes::<NonNull<RawNode>>(count)?,
                    )?
                }
                .cast::<NonNull<RawNode>>();
                unsafe { std::ptr::copy_nonoverlapping(children.as_ptr(), edges.as_ptr(), count) };
                let node = unsafe { allocate(&mut self.raw, NODE, size_of::<RawNode>())? }
                    .cast::<RawNode>();
                unsafe {
                    node.as_ptr().write(RawNode::Inner {
                        children: RawSlice::new(edges, count),
                        firsts: RawSlice::new(firsts, count),
                    })
                };
                parents.push(node);
            }
            std::mem::swap(&mut nodes, &mut parents);
        }
        let header =
            unsafe { allocate(&mut self.raw, TREE_BINDINGS, size_of::<RawTreeBindings>())? }
                .cast::<RawTreeBindings>();
        unsafe {
            header.as_ptr().write(RawTreeBindings {
                header: RawBindings {
                    pos: SrcPos::NONE,
                    len: header_len,
                },
                root: nodes[0],
            })
        };
        Ok(roots.push::<BindingsKind>(header.cast()))
    }

    /// Merge with whole-leaf sharing and a single collecting boundary. Both input owners
    /// remain rooted; after preflight, construction only calls the raw allocator
    /// (which cannot collect) and publishes the completed tree once. Separate
    /// leaf allocations allow later path updates to reclaim individual leaves.
    pub fn merge_tree_bindings<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        left: BindingsRoot<'_, 'h>,
        right: BindingsRoot<'_, 'h>,
    ) -> Result<BindingsRoot<'r, 'h>> {
        const FANOUT: usize = 64;
        // The sum bounds the union and all output allocations. Counting exact
        // duplicate names first would traverse overlapping leaves twice. This
        // bounded batch may collect conservatively when that upper bound reaches
        // the threshold; it never reserves the entire bound in the managed heap.
        let len = self
            .bindings_view(left)
            .len()
            .checked_add(self.bindings_view(right).len())
            .ok_or(Error::AllocationTooLarge)?;
        if len <= crate::bindings::FLAT_MAX {
            return Err(Error::InvalidRange);
        }
        // Identical balanced partitions to managed_bindings::even_chunks.
        fn groups(len: usize) -> impl Iterator<Item = Range<usize>> {
            let count = len.div_ceil(FANOUT);
            let base = len / count;
            let extra = len % count;
            let mut start = 0;
            (0..count).map(move |index| {
                let size = base + usize::from(index < extra);
                let range = start..start + size;
                start += size;
                range
            })
        }
        // Count leaf boundaries only, without scanning their attributes. Every
        // reused leaf can cause at most one extra partial copied leaf. With B
        // input leaves and T input attrs, copied leaves <= ceil(T/64)+B, and
        // total output leaves <= ceil(T/64)+2B (also at most T nonempty leaves).
        let leafs_in = |view| {
            let mut cursor = BindingsLeafCursor::new(view);
            let mut count = 0usize;
            while !cursor.leaf.is_empty() {
                count += 1;
                cursor.consume(cursor.leaf.len());
            }
            count
        };
        let input_leaves = leafs_in(self.bindings_view(left))
            .checked_add(leafs_in(self.bindings_view(right)))
            .ok_or(Error::AllocationTooLarge)?;
        let copied_leaves = len
            .div_ceil(FANOUT)
            .checked_add(input_leaves)
            .ok_or(Error::AllocationTooLarge)?
            .min(len);
        let leaf_count = copied_leaves
            .checked_add(input_leaves)
            .ok_or(Error::AllocationTooLarge)?
            .min(len);
        // RawAttr's size is a multiple of its descriptor alignment, so each
        // nonempty copied array adds the same prefix/padding to its payload.
        let leaf_overhead =
            self.raw.allocation_extent(ATTRS, size_of::<RawAttr>())? - size_of::<RawAttr>();
        let mut pending = array_bytes::<RawAttr>(len)?
            .checked_add(
                copied_leaves
                    .checked_mul(leaf_overhead)
                    .ok_or(Error::AllocationTooLarge)?,
            )
            .and_then(|bytes| {
                leaf_count
                    .checked_mul(size_of::<RawNode>())
                    .and_then(|nodes| bytes.checked_add(nodes))
            })
            .ok_or(Error::AllocationTooLarge)?;
        let mut account = |kind, bytes| -> Result<()> {
            pending = pending
                .checked_add(self.raw.allocation_extent(kind, bytes)?)
                .ok_or(Error::AllocationTooLarge)?;
            Ok(())
        };
        let mut count = leaf_count;
        while count > 1 {
            for range in groups(count) {
                account(SYMBOLS, array_bytes::<Symbol>(range.len())?)?;
                account(NODES, array_bytes::<NonNull<RawNode>>(range.len())?)?;
                account(NODE, size_of::<RawNode>())?;
            }
            count = count.div_ceil(FANOUT);
        }
        account(TREE_BINDINGS, size_of::<RawTreeBindings>())?;
        // Reserve both work vectors before entering the batch. Their capacities
        // suffice for all levels, so construction never grows a Vec or calls
        // arbitrary code. They contain only private unrooted node pointers.
        let mut nodes = Vec::<NonNull<RawNode>>::new();
        nodes
            .try_reserve_exact(leaf_count)
            .map_err(|_| Error::OutOfMemory)?;
        let mut parents = Vec::<NonNull<RawNode>>::new();
        parents
            .try_reserve_exact(leaf_count.div_ceil(FANOUT))
            .map_err(|_| Error::OutOfMemory)?;
        if self.raw.collection_due(pending) {
            self.collect_minor()?;
        }

        // No collecting operation is allowed below this point until the final
        // root is published. Raw allocation preserves existing object addresses;
        // even a refill only adds a disjoint allocation. Each payload is fully
        // initialized before the next allocation, so an OOM leaves only valid,
        // unreachable objects for the next collection or heap destruction.
        #[inline]
        unsafe fn allocate(raw: &mut RawHeap, kind: usize, bytes: usize) -> Result<NonNull<u8>> {
            #[cfg(test)]
            tree_merge_allocation_checkpoint()?;
            if let Some(pointer) = unsafe { raw.try_allocate(kind, bytes) } {
                return Ok(pointer);
            }
            unsafe { raw.allocate_slow(kind, bytes) }
        }
        // Source roots belong to this heap and were reloaded after the sole
        // collection point. These private views live only inside the batch;
        // no reference or unrooted pointer is returned through the safe API.
        let left: BindingsView<'_, 'h> = BindingsView::new(left.raw.read());
        let right: BindingsView<'_, 'h> = BindingsView::new(right.raw.read());
        let mut left = MergeLeafCursor::new(left);
        let mut right = MergeLeafCursor::new(right);
        struct MergeDecision {
            shared: Option<(NonNull<RawNode>, usize, usize, usize)>,
            left_disjoint: bool,
            right_disjoint: bool,
        }
        #[inline(always)]
        fn next_run(left: &MergeLeafCursor<'_>, right: &MergeLeafCursor<'_>) -> MergeDecision {
            let a = left.inner.leaf;
            let b = right.inner.leaf;
            let left_node = left.current_whole;
            let right_node = right.current_whole;
            if let Some(node) = left_node {
                if right_node == Some(node) {
                    return MergeDecision {
                        shared: Some((node, a.len(), b.len(), b.len())),
                        left_disjoint: false,
                        right_disjoint: false,
                    };
                }
            }
            let left_disjoint = !a.is_empty() && (b.is_empty() || a[a.len() - 1].name < b[0].name);
            let right_disjoint = !b.is_empty() && (a.is_empty() || b[b.len() - 1].name < a[0].name);
            let shared = if left_disjoint {
                left_node.map(|node| (node, a.len(), 0, a.len()))
            } else if right_disjoint {
                right_node.map(|node| (node, 0, b.len(), b.len()))
            } else {
                None
            };
            MergeDecision {
                shared,
                left_disjoint,
                right_disjoint,
            }
        }
        let mut output_len = 0usize;
        while !left.inner.leaf.is_empty() || !right.inner.leaf.is_empty() {
            let mut decision = next_run(&left, &right);
            if let Some((node, take_left, take_right, count)) = decision.shared {
                nodes.push(node);
                output_len += count;
                if take_left != 0 {
                    left.consume(take_left);
                }
                if take_right != 0 {
                    right.consume(take_right);
                }
                continue;
            }
            // No shared leaf is pending. Fill a bounded uncommitted payload
            // directly, stopping before the next reusable leaf. The guard owns
            // the raw heap borrow: neither allocation nor GC can intervene.
            // Cancellation exposes no object; RawAttr owns no drop resources.
            #[cfg(test)]
            tree_merge_allocation_checkpoint()?;
            let reservation = self
                .raw
                .reserve_variable(ATTRS, array_bytes::<RawAttr>(FANOUT)?)?;
            let destination = reservation.payload().cast::<RawAttr>().as_ptr();
            let mut written = 0usize;
            loop {
                let a = left.inner.leaf;
                let b = right.inner.leaf;
                if decision.left_disjoint {
                    let take = a.len().min(FANOUT - written);
                    unsafe {
                        std::ptr::copy_nonoverlapping(a.as_ptr(), destination.add(written), take);
                    }
                    left.consume(take);
                    written += take;
                    output_len += take;
                } else if decision.right_disjoint {
                    let take = b.len().min(FANOUT - written);
                    unsafe {
                        std::ptr::copy_nonoverlapping(b.as_ptr(), destination.add(written), take);
                    }
                    right.consume(take);
                    written += take;
                    output_len += take;
                } else {
                    let (mut i, mut j) = (0, 0);
                    while i < a.len() && j < b.len() && written < FANOUT {
                        let attr = if b[j].name <= a[i].name {
                            i += usize::from(a[i].name == b[j].name);
                            let attr = b[j];
                            j += 1;
                            attr
                        } else {
                            let attr = a[i];
                            i += 1;
                            attr
                        };
                        unsafe {
                            destination.add(written).write(attr);
                        }
                        written += 1;
                        output_len += 1;
                    }
                    left.consume(i);
                    right.consume(j);
                }
                if written == FANOUT || (left.inner.leaf.is_empty() && right.inner.leaf.is_empty())
                {
                    break;
                }
                decision = next_run(&left, &right);
                if decision.shared.is_some() {
                    break;
                }
            }
            // Publish only the initialized prefix, with its exact trace length,
            // before the next fallible node allocation. The unfilled tail stays
            // available to subsequent payloads and retains no pointer edges.
            let payload =
                unsafe { reservation.commit(array_bytes::<RawAttr>(written)?)? }.cast::<RawAttr>();
            let node =
                unsafe { allocate(&mut self.raw, NODE, size_of::<RawNode>())? }.cast::<RawNode>();
            unsafe {
                node.as_ptr()
                    .write(RawNode::Leaf(RawSlice::new(payload, written)));
            }
            nodes.push(node);
        }
        let header_len = u32::try_from(output_len).map_err(|_| Error::AllocationTooLarge)?;
        if output_len <= crate::bindings::FLAT_MAX {
            return Err(Error::InvalidRange);
        }
        while nodes.len() > 1 {
            parents.clear();
            for range in groups(nodes.len()) {
                let children = &nodes[range];
                let firsts = unsafe {
                    allocate(
                        &mut self.raw,
                        SYMBOLS,
                        array_bytes::<Symbol>(children.len())?,
                    )?
                }
                .cast::<Symbol>();
                for (index, child) in children.iter().enumerate() {
                    unsafe {
                        firsts.as_ptr().add(index).write(first_symbol(*child));
                    }
                }
                let edges = unsafe {
                    allocate(
                        &mut self.raw,
                        NODES,
                        array_bytes::<NonNull<RawNode>>(children.len())?,
                    )?
                }
                .cast::<NonNull<RawNode>>();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        children.as_ptr(),
                        edges.as_ptr(),
                        children.len(),
                    );
                }
                let node = unsafe { allocate(&mut self.raw, NODE, size_of::<RawNode>())? }
                    .cast::<RawNode>();
                unsafe {
                    node.as_ptr().write(RawNode::Inner {
                        children: RawSlice::new(edges, children.len()),
                        firsts: RawSlice::new(firsts, children.len()),
                    });
                }
                parents.push(node);
            }
            std::mem::swap(&mut nodes, &mut parents);
        }
        let header =
            unsafe { allocate(&mut self.raw, TREE_BINDINGS, size_of::<RawTreeBindings>())? }
                .cast::<RawTreeBindings>();
        unsafe {
            header.as_ptr().write(RawTreeBindings {
                header: RawBindings {
                    pos: SrcPos::NONE,
                    len: header_len,
                },
                root: nodes[0],
            });
        }
        Ok(roots.push::<BindingsKind>(header.cast()))
    }

    /// Merge two flat sets directly into one flat allocation, when their union
    /// fits. The right operand wins; a newly constructed result has no position.
    /// Empty-operand identity is handled by the bindings operation's caller.
    pub fn merge_flat_bindings<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        left: BindingsRoot<'_, 'h>,
        right: BindingsRoot<'_, 'h>,
    ) -> Result<Option<BindingsRoot<'r, 'h>>> {
        let left_len = self.bindings_view(left).len();
        let right_len = self.bindings_view(right).len();
        if left_len > crate::bindings::FLAT_MAX || right_len > crate::bindings::FLAT_MAX {
            return Ok(None);
        }
        // This helper is local to this operation: every call follows the flat
        // representation check above. Its slice lifetime forbids collection.
        fn attrs<'v, 'h>(heap: &'v Heap<'h>, set: BindingsRoot<'_, 'h>) -> &'v [RawAttr] {
            let view = heap.bindings_view(set);
            debug_assert!(view.len() <= crate::bindings::FLAT_MAX);
            // SAFETY: flat constructors initialize exactly header.len attrs at
            // this aligned offset, and the heap borrow keeps them stationary.
            unsafe {
                std::slice::from_raw_parts(
                    view.pointer
                        .as_ptr()
                        .cast::<u8>()
                        .add(FLAT_ATTRS_OFFSET)
                        .cast::<RawAttr>(),
                    view.len(),
                )
            }
        }
        let len = {
            let left = attrs(self, left);
            let right = attrs(self, right);
            let (mut i, mut j, mut shared) = (0, 0, 0);
            while i < left.len() && j < right.len() {
                let (a, b) = (left[i].name, right[j].name);
                i += usize::from(a <= b);
                j += usize::from(b <= a);
                shared += usize::from(a == b);
            }
            left_len + right_len - shared
        };
        if len > crate::bindings::FLAT_MAX {
            return Ok(None);
        }
        let layout = RawBindings::flat_layout(len).map_err(|_| Error::AllocationTooLarge)?;
        let pointer = self
            .reserve(FLAT_BINDINGS, layout.size())?
            .cast::<RawBindings>();
        // Reserve may move both operands. Reload their rooted owners here;
        // neither these borrows nor unrooted cell pointers cross another GC.
        let left = attrs(self, left);
        let right = attrs(self, right);
        let (mut i, mut j) = (0, 0);
        // SAFETY: pointer is a fresh disjoint allocation sized for len attrs.
        // The two sorted, unique inputs produce exactly len unique winners.
        // Every field is initialized before the result is rooted, and no
        // managed allocation occurs while writing the borrowed cell pointers.
        unsafe {
            let out = pointer
                .as_ptr()
                .cast::<u8>()
                .add(FLAT_ATTRS_OFFSET)
                .cast::<RawAttr>();
            for index in 0..len {
                let attr = if j < right.len() && (i == left.len() || right[j].name <= left[i].name)
                {
                    if i < left.len() && left[i].name == right[j].name {
                        i += 1;
                    }
                    let attr = right[j];
                    j += 1;
                    attr
                } else {
                    let attr = left[i];
                    i += 1;
                    attr
                };
                out.add(index).write(attr);
            }
            pointer.as_ptr().write(RawBindings {
                pos: SrcPos::NONE,
                len: len as u32,
            });
        }
        Ok(Some(roots.push::<BindingsKind>(pointer)))
    }

    pub fn alloc_flat_bindings<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        pos: SrcPos,
        attrs: &[RootedAttr<'_, 'h>],
    ) -> Result<BindingsRoot<'r, 'h>> {
        if attrs.len() > crate::bindings::FLAT_MAX {
            return Err(Error::InvalidRange);
        }
        if attrs.is_empty() && pos == SrcPos::NONE {
            if let Some(set) = self.empty_attrs.borrow().as_ref() {
                return Ok(roots.copy(set.root()));
            }
        }
        validate_attrs(attrs)?;
        let layout =
            RawBindings::flat_layout(attrs.len()).map_err(|_| Error::AllocationTooLarge)?;
        let pointer = self
            .reserve(FLAT_BINDINGS, layout.size())?
            .cast::<RawBindings>();
        unsafe {
            pointer.as_ptr().write(RawBindings {
                pos,
                len: attrs.len() as u32,
            });
            let out = pointer
                .as_ptr()
                .cast::<u8>()
                .add(FLAT_ATTRS_OFFSET)
                .cast::<RawAttr>();
            for (index, attr) in attrs.iter().enumerate() {
                out.add(index).write(RawAttr {
                    name: attr.name,
                    pos: attr.pos,
                    value: attr.value.raw.read(),
                });
            }
        }
        let set = roots.push::<BindingsKind>(pointer);
        if attrs.is_empty() && pos == SrcPos::NONE {
            *self.empty_attrs.borrow_mut() = Some(self.persist(set));
        }
        Ok(set)
    }
    pub fn alloc_leaf<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        attrs: &[RootedAttr<'_, 'h>],
    ) -> Result<NodeRoot<'r, 'h>> {
        if attrs.is_empty() || attrs.len() > 64 {
            return Err(Error::InvalidRange);
        }
        validate_attrs(attrs)?;
        let pointer = self
            .reserve(ATTRS, array_bytes::<RawAttr>(attrs.len())?)?
            .cast::<RawAttr>();
        for (index, attr) in attrs.iter().enumerate() {
            unsafe {
                pointer.as_ptr().add(index).write(RawAttr {
                    name: attr.name,
                    pos: attr.pos,
                    value: attr.value.raw.read(),
                });
            }
        }
        let attrs = roots.push::<AttrsKind>(RawSlice::new(pointer, attrs.len()));
        // SAFETY: NODE has the unpadded fixed layout of RawNode.
        let node =
            unsafe { self.reserve_fixed::<{ size_of::<RawNode>() }>(NODE)? }.cast::<RawNode>();
        unsafe { node.as_ptr().write(RawNode::Leaf(attrs.raw.read())) };
        Ok(roots.push::<NodeKind>(node))
    }
    pub fn alloc_inner<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        children: &[NodeRoot<'_, 'h>],
    ) -> Result<NodeRoot<'r, 'h>> {
        if children.is_empty() || children.len() > 64 {
            return Err(Error::InvalidRange);
        }
        let firsts: Vec<Symbol> = children
            .iter()
            .map(|child| unsafe { first_symbol(child.raw.read()) })
            .collect();
        if firsts.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Error::InvalidRange);
        }
        let pointer = self
            .reserve(SYMBOLS, array_bytes::<Symbol>(firsts.len())?)?
            .cast::<Symbol>();
        unsafe { std::ptr::copy_nonoverlapping(firsts.as_ptr(), pointer.as_ptr(), firsts.len()) };
        let firsts_root = roots.push::<BytesKind>(RawSlice::new(
            pointer.cast(),
            array_bytes::<Symbol>(firsts.len())?,
        ));
        let pointer = self
            .reserve(NODES, array_bytes::<NonNull<RawNode>>(children.len())?)?
            .cast::<NonNull<RawNode>>();
        for (index, child) in children.iter().enumerate() {
            unsafe { pointer.as_ptr().add(index).write(child.raw.read()) };
        }
        let children_root = roots.push::<NodesKind>(RawSlice::new(pointer, children.len()));
        // SAFETY: NODE has the unpadded fixed layout of RawNode.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawNode>() }>(NODE)? }.cast::<RawNode>();
        unsafe {
            pointer.as_ptr().write(RawNode::Inner {
                children: children_root.raw.read(),
                firsts: RawSlice::new(firsts_root.raw.read().ptr.cast(), firsts.len()),
            })
        };
        Ok(roots.push::<NodeKind>(pointer))
    }
    pub fn alloc_tree_bindings<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        pos: SrcPos,
        len: usize,
        node: NodeRoot<'_, 'h>,
    ) -> Result<BindingsRoot<'r, 'h>> {
        if len <= crate::bindings::FLAT_MAX {
            return Err(Error::InvalidRange);
        }
        let len = u32::try_from(len).map_err(|_| Error::AllocationTooLarge)?;
        // SAFETY: TREE_BINDINGS has the unpadded fixed layout of RawTreeBindings.
        let pointer =
            unsafe { self.reserve_fixed::<{ size_of::<RawTreeBindings>() }>(TREE_BINDINGS)? }
                .cast::<RawTreeBindings>();
        unsafe {
            pointer.as_ptr().write(RawTreeBindings {
                header: RawBindings { pos, len },
                root: node.raw.read(),
            })
        };
        Ok(roots.push::<BindingsKind>(pointer.cast()))
    }
}
#[cfg(test)]
std::thread_local! {
    static FAIL_SLOW_RESERVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Fail a slow reservation after its collecting boundary, before allocating.
#[cfg(test)]
fn with_slow_reservation_failure<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FAIL_SLOW_RESERVATION.set(self.0);
        }
    }
    let _restore = Restore(FAIL_SLOW_RESERVATION.replace(true));
    f()
}

#[cfg(test)]
std::thread_local! {
    static TREE_MERGE_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Test only: fail before the nth batch allocation, after all preceding
/// allocations have been initialized. Scope restores the previous injection.
#[cfg(test)]
pub(crate) fn with_tree_merge_failure<R>(after: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TREE_MERGE_FAILURE.set(self.0);
        }
    }
    let _restore = Restore(TREE_MERGE_FAILURE.replace(Some(after)));
    f()
}

#[cfg(test)]
fn tree_merge_allocation_checkpoint() -> Result<()> {
    match TREE_MERGE_FAILURE.get() {
        Some(0) => Err(Error::OutOfMemory),
        Some(remaining) => {
            TREE_MERGE_FAILURE.set(Some(remaining - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
std::thread_local! {
    static CONCAT_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn with_concat_failure<R>(after: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONCAT_FAILURE.set(self.0);
        }
    }
    let _restore = Restore(CONCAT_FAILURE.replace(Some(after)));
    f()
}
#[cfg(test)]
fn concat_allocation_checkpoint() -> Result<()> {
    match CONCAT_FAILURE.get() {
        Some(0) => Err(Error::OutOfMemory),
        Some(remaining) => {
            CONCAT_FAILURE.set(Some(remaining - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
std::thread_local! {
    static FLAT_THUNK_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn with_flat_thunk_failure<R>(after: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FLAT_THUNK_FAILURE.set(self.0);
        }
    }
    let _restore = Restore(FLAT_THUNK_FAILURE.replace(Some(after)));
    f()
}
#[cfg(test)]
fn flat_thunk_allocation_checkpoint() -> Result<()> {
    match FLAT_THUNK_FAILURE.get() {
        Some(0) => Err(Error::OutOfMemory),
        Some(remaining) => {
            FLAT_THUNK_FAILURE.set(Some(remaining - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
std::thread_local! {
    static MAP_ATTRS_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Test only: fail before the nth batch allocation, after all preceding
/// allocations have been initialized. Scope restores the previous injection.
#[cfg(test)]
pub(crate) fn with_map_attrs_failure<R>(after: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            MAP_ATTRS_FAILURE.set(self.0);
        }
    }
    let _restore = Restore(MAP_ATTRS_FAILURE.replace(Some(after)));
    f()
}

#[cfg(test)]
fn map_attrs_allocation_checkpoint() -> Result<()> {
    match MAP_ATTRS_FAILURE.get() {
        Some(0) => Err(Error::OutOfMemory),
        Some(remaining) => {
            MAP_ATTRS_FAILURE.set(Some(remaining - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

fn validate_attrs(attrs: &[RootedAttr<'_, '_>]) -> Result<()> {
    if attrs.windows(2).any(|pair| pair[0].name >= pair[1].name) {
        Err(Error::InvalidRange)
    } else {
        Ok(())
    }
}
unsafe fn first_symbol(node: NonNull<RawNode>) -> Symbol {
    unsafe {
        match *node.as_ref() {
            RawNode::Leaf(attrs) => slice(attrs)[0].name,
            RawNode::Inner { firsts, .. } => slice(firsts)[0],
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct AttrView<'v, 'h> {
    pub name: Symbol,
    pub pos: SrcPos,
    pub value: CellView<'v, 'h>,
}
impl<'h> AttrView<'_, 'h> {
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> RootedAttr<'r, 'h> {
        RootedAttr {
            name: self.name,
            pos: self.pos,
            value: self.value.root(roots),
        }
    }
}
/// Whole-leaf identity cache used only by merging. Other batch operations keep
/// the smaller cursor, and no cached pointer survives the input view's borrow.
struct MergeLeafCursor<'v> {
    inner: BindingsLeafCursor<'v>,
    current_whole: Option<NonNull<RawNode>>,
}
impl<'v> MergeLeafCursor<'v> {
    fn new(view: BindingsView<'v, '_>) -> Self {
        let inner = BindingsLeafCursor::new(view);
        let current_whole = inner.whole_leaf_node();
        Self {
            inner,
            current_whole,
        }
    }

    #[inline]
    fn consume(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let previous_len = self.inner.leaf.len();
        self.inner.consume(count);
        self.current_whole = if count == previous_len {
            self.inner.whole_leaf_node()
        } else {
            None
        };
    }
}

/// Private cursor for batch operations over complete leaf slices. Unlike the
/// public iterator it has no cached attribute/Option state or per-item counter.
/// Its references are bounded by the input view; batching creates views only
/// after preflight and never collects while cursors exist.
struct BindingsLeafCursor<'v> {
    stack: SmallVec<[(&'v RawNode, usize); 8]>,
    leaf: &'v [RawAttr],
}
impl<'v> BindingsLeafCursor<'v> {
    /// Only complete tree leaves can be reused. An interior slice would retain
    /// every cell in its allocation owner, including discarded attributes.
    fn whole_leaf_node(&self) -> Option<NonNull<RawNode>> {
        let &(parent, next) = self.stack.last()?;
        let RawNode::Inner { children, .. } = parent else {
            unreachable!("cursor parents are inner nodes")
        };
        let node = unsafe { slice(*children)[next - 1] };
        let RawNode::Leaf(attrs) = (unsafe { node.as_ref() }) else {
            unreachable!("innermost parent selects the current leaf")
        };
        (attrs.ptr.as_ptr() == self.leaf.as_ptr().cast_mut() && attrs.len == self.leaf.len())
            .then_some(node)
    }

    // Resume using only immutable symbol metadata across collections. Rebuild
    // the borrowed traversal path after each safepoint, never retain raw views.
    fn after(view: BindingsView<'v, '_>, after: Option<Symbol>) -> Self {
        let Some(after) = after else {
            return Self::new(view);
        };
        let mut cursor = if view.len() <= crate::bindings::FLAT_MAX {
            Self::new(view)
        } else {
            let mut cursor = Self {
                stack: SmallVec::new(),
                leaf: &[],
            };
            let mut node = unsafe {
                view.pointer
                    .cast::<RawTreeBindings>()
                    .as_ref()
                    .root
                    .as_ref()
            };
            loop {
                match node {
                    RawNode::Leaf(attrs) => {
                        cursor.leaf = unsafe { slice(*attrs) };
                        break;
                    }
                    RawNode::Inner { children, firsts } => {
                        let index = unsafe { slice(*firsts) }
                            .partition_point(|first| *first <= after)
                            .max(1)
                            - 1;
                        cursor.stack.push((node, index + 1));
                        node = unsafe { slice(*children)[index].as_ref() };
                    }
                }
            }
            cursor
        };
        let consumed = cursor.leaf.partition_point(|attr| attr.name <= after);
        cursor.consume(consumed);
        cursor
    }

    fn new(view: BindingsView<'v, '_>) -> Self {
        let mut cursor = Self {
            stack: SmallVec::new(),
            leaf: &[],
        };
        if view.len() <= crate::bindings::FLAT_MAX {
            // SAFETY: flat constructors initialize header.len attributes at
            // this aligned offset, and the source view keeps them stationary.
            cursor.leaf = unsafe {
                std::slice::from_raw_parts(
                    view.pointer
                        .as_ptr()
                        .cast::<u8>()
                        .add(FLAT_ATTRS_OFFSET)
                        .cast::<RawAttr>(),
                    view.len(),
                )
            };
        } else {
            cursor.descend(unsafe {
                view.pointer
                    .cast::<RawTreeBindings>()
                    .as_ref()
                    .root
                    .as_ref()
            });
        }
        cursor
    }

    fn descend(&mut self, mut node: &'v RawNode) {
        loop {
            match node {
                RawNode::Leaf(attrs) => {
                    self.leaf = unsafe { slice(*attrs) };
                    return;
                }
                RawNode::Inner { children, .. } => {
                    // Any unusually deep stack spill uses only the ordinary
                    // allocator, never a managed allocation, GC or callback.
                    self.stack.push((node, 1));
                    node = unsafe { slice(*children)[0].as_ref() };
                }
            }
        }
    }

    #[inline]
    fn consume(&mut self, count: usize) {
        self.leaf = &self.leaf[count..];
        if self.leaf.is_empty() {
            self.advance();
        }
    }

    #[inline(never)]
    fn advance(&mut self) {
        while let Some((node, next)) = self.stack.last_mut() {
            let RawNode::Inner { children, .. } = node else {
                unreachable!("inner cursor node")
            };
            if *next < children.len {
                let child = unsafe { slice(*children)[*next].as_ref() };
                *next += 1;
                self.descend(child);
                return;
            }
            self.stack.pop();
        }
    }
}

pub(crate) struct BindingsIter<'v, 'h> {
    // These references are bounded by the immutable Heap borrow of the view;
    // allocation and collection require &mut Heap and cannot overlap iteration.
    // Only inner nodes appear on the stack, with their next child index.
    stack: SmallVec<[(&'v RawNode, usize); 8]>,
    leaf: std::slice::Iter<'v, RawAttr>,
    remaining: usize,
    borrow: PhantomData<&'v Heap<'h>>,
}
impl<'v, 'h> BindingsIter<'v, 'h> {
    fn descend(&mut self, mut node: &'v RawNode) {
        loop {
            match node {
                RawNode::Leaf(attrs) => {
                    // SAFETY: node and all of its descendants belong to the
                    // immutably borrowed heap and remain alive and stationary.
                    self.leaf = unsafe { slice(*attrs) }.iter();
                    return;
                }
                RawNode::Inner { children, .. } => {
                    self.stack.push((node, 1));
                    // Inner construction rejects an empty child slice.
                    node = unsafe { slice(*children)[0].as_ref() };
                }
            }
        }
    }

    // Keep traversal out of the per-attribute fast path. Leaves contain up to
    // 64 attributes, so this is called only at boundaries and on exhaustion.
    #[inline(never)]
    fn advance(&mut self) -> Option<&'v RawAttr> {
        while let Some((node, next)) = self.stack.last_mut() {
            let RawNode::Inner { children, .. } = node else {
                unreachable!("only inner nodes are stacked")
            };
            if *next < children.len {
                let child = unsafe { slice(*children)[*next].as_ref() };
                *next += 1;
                self.descend(child);
                // Leaf construction rejects empty attribute slices.
                return self.leaf.next();
            }
            self.stack.pop();
        }
        None
    }
}
impl<'v, 'h> Iterator for BindingsIter<'v, 'h> {
    type Item = AttrView<'v, 'h>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let attr = match self.leaf.next() {
            Some(attr) => attr,
            None => self.advance()?,
        };
        self.remaining -= 1;
        Some(AttrView {
            name: attr.name,
            pos: attr.pos,
            value: CellView::new(attr.value),
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for BindingsIter<'_, '_> {}
impl std::iter::FusedIterator for BindingsIter<'_, '_> {}
impl<'v, 'h> BindingsView<'v, 'h> {
    /// Root a bounded ordered batch for comparisons that may collect between
    /// elements. The continuation contains names only; no heap view escapes.
    pub fn comparison_batch<'r>(
        self,
        other: Self,
        roots: &Roots<'r, 'h>,
        after: Option<(Symbol, Symbol)>,
    ) -> SmallVec<[(Symbol, Symbol, CellRoot<'r, 'h>, CellRoot<'r, 'h>); 32]> {
        let mut left = BindingsLeafCursor::after(self, after.map(|pair| pair.0));
        let mut right = BindingsLeafCursor::after(other, after.map(|pair| pair.1));
        let mut pairs = SmallVec::new();
        while pairs.len() < 32 {
            let (Some(a), Some(b)) = (left.leaf.first(), right.leaf.first()) else {
                break;
            };
            pairs.push((
                a.name,
                b.name,
                roots.push::<CellKind>(a.value),
                roots.push::<CellKind>(b.value),
            ));
            left.consume(1);
            right.consume(1);
        }
        pairs
    }

    pub fn len(self) -> usize {
        unsafe { self.pointer.as_ref().len as usize }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn pos(self) -> SrcPos {
        unsafe { self.pointer.as_ref().pos }
    }
    pub fn root<'r>(self, roots: &Roots<'r, 'h>) -> BindingsRoot<'r, 'h> {
        roots.push::<BindingsKind>(self.pointer)
    }
    pub fn iter(self) -> BindingsIter<'v, 'h> {
        let len = self.len();
        let mut iter = BindingsIter {
            stack: SmallVec::new(),
            leaf: [].iter(),
            remaining: len,
            borrow: PhantomData,
        };
        if len <= crate::bindings::FLAT_MAX {
            // SAFETY: the inline attributes are initialized by the flat
            // constructor, and the view carries their immutable heap borrow.
            iter.leaf = unsafe {
                std::slice::from_raw_parts(
                    self.pointer
                        .as_ptr()
                        .cast::<u8>()
                        .add(FLAT_ATTRS_OFFSET)
                        .cast::<RawAttr>(),
                    len,
                )
            }
            .iter();
        } else {
            let root = unsafe {
                self.pointer
                    .cast::<RawTreeBindings>()
                    .as_ref()
                    .root
                    .as_ref()
            };
            iter.descend(root);
        }
        iter
    }
    /// Keep field projection at the call site: existence checks need no field
    /// loads, and value-only lookups need not copy the name and source position.
    #[inline]
    pub fn get(self, name: Symbol) -> Option<AttrView<'v, 'h>> {
        self.lookup_attr(name).map(|attr| AttrView {
            name: attr.name,
            pos: attr.pos,
            value: CellView::new(attr.value),
        })
    }

    // Return one borrowed pointer from the shared search routine, rather than
    // writing a complete AttrView into a caller-provided return buffer.
    fn lookup_attr(self, name: Symbol) -> Option<&'v RawAttr> {
        let len = self.len();
        let attrs = if len <= crate::bindings::FLAT_MAX {
            // SAFETY: the flat header owns len initialized inline attributes.
            // Its view bounds the returned borrow and excludes moving the heap.
            unsafe {
                std::slice::from_raw_parts(
                    self.pointer
                        .as_ptr()
                        .cast::<u8>()
                        .add(FLAT_ATTRS_OFFSET)
                        .cast::<RawAttr>(),
                    len,
                )
            }
        } else {
            // SAFETY: this tree and every child remain stationary for 'v.
            let mut node = unsafe {
                self.pointer
                    .cast::<RawTreeBindings>()
                    .as_ref()
                    .root
                    .as_ref()
            };
            loop {
                match node {
                    RawNode::Leaf(attrs) => break unsafe { slice(*attrs) },
                    RawNode::Inner { children, firsts } => {
                        let firsts = unsafe { slice(*firsts) };
                        // These fixed, low symbol IDs need at most four
                        // comparisons, even in a very wide tree node.
                        let index = if name <= crate::symbol::sym::TYPE {
                            firsts
                                .iter()
                                .take_while(|first| **first <= name)
                                .count()
                                .checked_sub(1)?
                        } else {
                            firsts
                                .partition_point(|first| *first <= name)
                                .checked_sub(1)?
                        };
                        node = unsafe { slice(*children)[index].as_ref() };
                    }
                }
            }
        };
        if name <= crate::symbol::sym::TYPE {
            attrs
                .iter()
                .take_while(|attr| attr.name <= name)
                .find(|attr| attr.name == name)
        } else if attrs.len() <= 8 {
            attrs.iter().find(|attr| attr.name == name)
        } else {
            attrs
                .binary_search_by_key(&name, |attr| attr.name)
                .ok()
                .map(|index| &attrs[index])
        }
    }
}

#[cfg(all(test, not(brix_parallel)))]
#[path = "managed_heap_tests.rs"]
mod tests;

#[cfg(feature = "work-counters")]
mod equality_profile;

mod attribute_cache;
mod derivation_cache;
