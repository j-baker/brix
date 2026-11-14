//! Raw backend for a precise, single-threaded moving collector.
//!
//! This module deliberately does not provide the safe root/view interface. Its
//! caller owns that interface, suspends every heap view during collection, and
//! enumerates all strong roots and pointer fields exactly. Fixed-size kinds use
//! headerless typed chunks. Variable-size kinds store a single size word before
//! each payload (padded to its alignment). Forwarding and object-boundary tables
//! are prepared only when collecting, using bounded reusable scratch storage.
//!
//! Collection reserves destinations, forwarding records and a mark worklist before
//! changing roots. Thus a returned allocation error leaves the graph intact.
//! Trace, weak-update and drop callbacks must not unwind or reenter the heap.
//! Copying transfers ownership: only unreachable source payloads are dropped;
//! forwarded source copies are never dropped. Dropping the heap drops every
//! remaining payload exactly once. Only collection reads the clock; wasm
//! reports zero collection time and requires no clock support.
//!
//! Allocations enter a copying nursery. Survivors are promoted into segregated
//! mark-and-compact slabs; minor collections trace roots and remembered old
//! objects, while major collections also trace and compact the old generation.
//! Mutating a managed edge requires [`RawHeap::write_barrier`]. Addresses may
//! change at any collection; callers must continue to root and reload pointers.

mod old;
mod scratch;
use old::{Compaction, OldHeap};
use scratch::Scratch;
#[cfg(not(target_family = "wasm"))]
mod parallel;
#[cfg(not(target_family = "wasm"))]
mod pool;

use std::alloc::{alloc, dealloc, Layout};
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{Config, Error, Result, Stats};

type Pointer = NonNull<u8>;

/// The layout and exact outgoing edges of one allocation kind.
///
/// Callbacks receive the payload address and its logical byte length, excluding
/// prefixes and alignment padding. They may access only those bytes. `trace`
/// rewrites every managed pointer field exactly once per invocation, including
/// interior pointers, through [`Tracer`]. Callbacks can run twice during a major
/// collection. Never dereference a rewritten edge: compaction destinations are
/// not initialized until all pointer repair finishes. It must not interpret integers as pointers. External references
/// may be left alone, provided their owners outlive every use by the heap.
///
/// Payloads must be movable by copying their bytes: no address-dependent state,
/// pinned values, or references into their own allocation unless traced. `drop`
/// must release only resources owned by this payload, never recursively destroy
/// managed children. None of these callbacks may unwind, allocate in this heap,
/// or expose pointers to application code.
pub struct Descriptor {
    pub name: &'static str,
    pub fixed_size: Option<usize>,
    pub alignment: usize,
    pub trace: unsafe fn(Pointer, usize, &mut Tracer<'_>),
    pub drop: Option<unsafe fn(Pointer, usize)>,
}

struct Chunk {
    pointer: Pointer,
    layout: Layout,
    used: usize,
    // Cumulative variable-object count when this chunk was opened. This gives
    // disjoint metadata slices without adding work to the allocation fast path.
    first_variable: usize,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // SAFETY: this chunk exclusively owns an allocation with this layout.
        unsafe { dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

/// Nursery chunks kept between collections.
///
/// A collection empties the nursery, and the mutator refills it at once
/// with chunks of the same size. Handing the old ones back to the
/// allocator in between makes it return their pages to the operating
/// system and take fresh ones, each of which faults in and is zeroed
/// again: on an evaluation that collects every half second that was a
/// tenth of its CPU time. Only chunks of the configured size and the
/// alignment `Space::add_chunk` gives large chunks are kept, up to the
/// nursery budget that is about to be refilled; oversized chunks and the
/// excess of a larger start-up nursery go back to the allocator.
struct SpareChunks {
    layout: Option<Layout>,
    pointers: Vec<Pointer>,
}

impl SpareChunks {
    fn new(chunk_bytes: usize) -> Self {
        Self {
            layout: Layout::from_size_align(chunk_bytes, 4096).ok(),
            pointers: Vec::new(),
        }
    }

    /// A kept chunk of exactly this layout, if there is one.
    fn take(&mut self, layout: Layout) -> Option<Pointer> {
        if self.layout == Some(layout) {
            self.pointers.pop()
        } else {
            None
        }
    }

    fn bytes(&self) -> usize {
        self.layout
            .map_or(0, |layout| layout.size() * self.pointers.len())
    }

    /// Keeps the chunk if it has the pooled layout and fits under `limit`
    /// bytes; otherwise it is freed.
    fn keep(&mut self, chunk: Chunk, limit: usize) {
        if self.layout == Some(chunk.layout)
            && self.bytes().saturating_add(chunk.layout.size()) <= limit
            && self.pointers.try_reserve(1).is_ok()
        {
            self.pointers.push(chunk.pointer);
            std::mem::forget(chunk);
        }
    }
}

impl Drop for SpareChunks {
    fn drop(&mut self) {
        if let Some(layout) = self.layout {
            for pointer in self.pointers.drain(..) {
                // SAFETY: `keep` took ownership of an allocation of this layout.
                unsafe { dealloc(pointer.as_ptr(), layout) };
            }
        }
    }
}

// SAFETY: the pool exclusively owns the allocations its pointers name.
unsafe impl Send for SpareChunks {}

/// A pool of spare nursery chunks that several heaps may share.
///
/// Mutators that evaluate in parallel each allocate from their own
/// nursery, and one heap collects them all
/// ([`RawHeap::absorb_nursery`]). The chunks a collection empties
/// therefore end up in the collecting heap, while the heaps that need
/// chunks next are the others: all of them must take from one pool, or
/// the collector's pool only holds memory nobody uses. The lock is taken
/// when a nursery space opens a chunk and when a collection ends.
#[derive(Clone)]
pub struct SpareChunkPool(Arc<Mutex<SpareChunks>>);

impl SpareChunkPool {
    /// An empty pool for heaps with this configuration's chunk size.
    pub fn new(config: &Config) -> Self {
        Self(Arc::new(Mutex::new(SpareChunks::new(config.chunk_bytes))))
    }

    fn lock(&self) -> MutexGuard<'_, SpareChunks> {
        // A chunk list is consistent between any two of its operations.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct Space {
    chunks: Vec<Chunk>,
    cursor: Pointer,
    remaining: usize,
    active_capacity: usize,
    retired_used: usize,
    variable_count: usize,
    capacity: usize,
    alignment: usize,
    prefix: usize,
    fixed_size: Option<usize>,
    fixed_extent: Option<usize>,
}

impl Space {
    fn new(descriptor: &Descriptor) -> Result<Self> {
        // Layout validates power-of-two alignment and representable sizes.
        Layout::from_size_align(1, descriptor.alignment).map_err(|_| Error::InvalidRange)?;
        let alignment = if descriptor.fixed_size.is_none() {
            descriptor.alignment.max(align_of::<usize>())
        } else {
            descriptor.alignment
        };
        let prefix = if descriptor.fixed_size.is_none() {
            round_up(size_of::<usize>(), alignment)?
        } else {
            0
        };
        let mut space = Self {
            chunks: Vec::new(),
            cursor: Pointer::dangling(),
            remaining: 0,
            active_capacity: 0,
            retired_used: 0,
            variable_count: 0,
            capacity: 0,
            alignment,
            prefix,
            fixed_size: descriptor.fixed_size,
            fixed_extent: None,
        };
        if let Some(bytes) = descriptor.fixed_size {
            space.fixed_extent = Some(space.extent(bytes)?);
        }
        Ok(space)
    }

    #[inline]
    fn extent(&self, bytes: usize) -> Result<usize> {
        if self.fixed_size.is_some_and(|fixed| fixed != bytes) {
            return Err(Error::InvalidRange);
        }
        if let Some(extent) = self.fixed_extent {
            return Ok(extent);
        }
        // Even an empty payload has a unique, in-bounds owner address.
        let size = bytes
            .max(1)
            .checked_add(self.prefix)
            .ok_or(Error::AllocationTooLarge)?;
        let extent = round_up(size, self.alignment)?;
        Layout::from_size_align(extent, self.alignment).map_err(|_| Error::AllocationTooLarge)?;
        Ok(extent)
    }

    /// Only for published allocations whose layout was checked at allocation.
    #[inline]
    fn extent_known(&self, bytes: usize) -> usize {
        self.fixed_extent.unwrap_or_else(|| {
            (bytes.max(1) + self.prefix + self.alignment - 1) & !(self.alignment - 1)
        })
    }

    #[inline]
    fn try_allocate(&mut self, bytes: usize, extent: usize) -> Option<Pointer> {
        if extent > self.remaining {
            return None;
        }
        let start = self.cursor;
        // SAFETY: extent fits the remaining chunk; prefix and payload fit it.
        let payload = unsafe {
            self.cursor = Pointer::new_unchecked(start.as_ptr().add(extent));
            if self.prefix != 0 {
                start.cast::<usize>().as_ptr().write(bytes);
                self.variable_count += 1;
            }
            Pointer::new_unchecked(start.as_ptr().add(self.prefix))
        };
        self.remaining -= extent;
        Some(payload)
    }

    fn add_chunk(&mut self, bytes: usize, spare: &mut SpareChunks) -> Result<()> {
        // Large chunks commonly sit exactly on allocator size boundaries.
        // In mimalloc v3, requesting ordinary alignment can overallocate them
        // by alignment-1 and round up to another size class; 4KiB alignment
        // selects its naturally aligned large-block path. This is an allocator
        // hint, not an OS page-size assumption or a change to object strides.
        let chunk_alignment = if bytes >= 1 << 20 {
            self.alignment.max(4096)
        } else {
            self.alignment
        };
        let layout = Layout::from_size_align(bytes, chunk_alignment)
            .map_err(|_| Error::AllocationTooLarge)?;
        let capacity = self
            .capacity
            .checked_add(bytes)
            .ok_or(Error::AllocationTooLarge)?;
        #[cfg(test)]
        reservation_checkpoint()?;
        self.chunks.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        #[cfg(test)]
        reservation_checkpoint()?;
        // SAFETY: nonempty validated layout. Failure leaves this space intact.
        let pointer = match spare.take(layout) {
            Some(pointer) => pointer,
            None => Pointer::new(unsafe { alloc(layout) }).ok_or(Error::OutOfMemory)?,
        };
        let active_used = self.active_capacity - self.remaining;
        if let Some(last) = self.chunks.last_mut() {
            last.used = active_used;
        }
        self.retired_used += active_used;
        self.cursor = pointer;
        self.remaining = bytes;
        self.active_capacity = bytes;
        self.capacity = capacity;
        self.chunks.push(Chunk {
            pointer,
            layout,
            used: 0,
            first_variable: self.variable_count,
        });
        Ok(())
    }

    fn used(&self) -> usize {
        self.retired_used + self.active_capacity - self.remaining
    }

    /// Every initialized payload, without allocating boundary records.
    fn objects(&self, mut f: impl FnMut(Pointer, usize, usize)) {
        for (index, chunk) in self.chunks.iter().enumerate() {
            let used = if index + 1 == self.chunks.len() {
                self.active_capacity - self.remaining
            } else {
                chunk.used
            };
            let mut offset = 0;
            while offset < used {
                // SAFETY: chunks contain only complete allocations. A variable
                // allocation's size prefix is initialized by try_allocate.
                let (payload, bytes) = unsafe {
                    let start = chunk.pointer.as_ptr().add(offset);
                    let bytes = self
                        .fixed_size
                        .unwrap_or_else(|| start.cast::<usize>().read());
                    (Pointer::new_unchecked(start.add(self.prefix)), bytes)
                };
                let extent = self.extent_known(bytes);
                f(payload, bytes, extent);
                offset += extent;
            }
            debug_assert_eq!(offset, used);
        }
    }
}

#[inline]
fn round_up(bytes: usize, alignment: usize) -> Result<usize> {
    bytes
        .checked_add(alignment - 1)
        .map(|n| n & !(alignment - 1))
        .ok_or(Error::AllocationTooLarge)
}

/// An unrooted raw heap. See the module-level safety contract.
pub struct RawHeap {
    descriptors: &'static [Descriptor],
    spaces: Vec<Space>,
    config: Config,
    threshold: usize,
    // Upper bound on occupied bytes, updated only when chunks change. Batch
    // safepoints below this bound avoid walking every kind's bump cursor.
    capacity: usize,
    old: OldHeap,
    major_threshold: usize,
    major_growth: usize,
    stats: Stats,
    scratch: Scratch,
    spare: SpareChunkPool,
    #[cfg(not(target_family = "wasm"))]
    parallel_tracing: bool,
    #[cfg(not(target_family = "wasm"))]
    pool: Option<pool::Pool>,
}

/// Exclusive reservation for a variable-size payload which is not yet an
/// object. Dropping or forgetting this guard cancels the reservation; neither
/// the payload's size prefix nor the space's occupied extent has changed.
/// Successfully reserved chunks remain available for later allocations.
///
/// The borrow prevents allocation or collection while filling the payload:
/// ```compile_fail
/// use brix_gc::raw::RawHeap;
/// fn overlapping(heap: &mut RawHeap) {
///     let pending = heap.reserve_variable(0, 64).unwrap();
///     unsafe { heap.allocate_slow(0, 8).unwrap(); }
///     let _ = pending.payload();
/// }
/// ```
#[must_use = "initialize and commit the payload, or drop to cancel"]
pub struct VariableReservation<'heap> {
    space: &'heap mut Space,
    max_bytes: usize,
}

impl Drop for VariableReservation<'_> {
    fn drop(&mut self) {
        // Cancellation needs no rollback: the cursor has never advanced. After
        // commit, it already describes the fully initialized published object.
    }
}

impl VariableReservation<'_> {
    /// Address of the writable payload, aligned for this reservation's kind.
    /// Only the requested maximum byte range may be accessed. The address is
    /// valid until cancellation, or until the committed object is moved/freed.
    #[inline]
    pub fn payload(&self) -> Pointer {
        // SAFETY: reserve_variable has reserved the maximum complete extent.
        unsafe { Pointer::new_unchecked(self.space.cursor.as_ptr().add(self.space.prefix)) }
    }

    /// Publish exactly the initialized prefix as one variable-size object.
    /// Invalid lengths cancel the reservation without exposing an object.
    ///
    /// # Safety
    /// The first `initialized_bytes` payload bytes must satisfy the descriptor,
    /// including its logical length and all managed edges. No payload may be
    /// pinned. Edges must name initialized objects in this heap (or traced
    /// interior pointers into this committed prefix); external edges must stay
    /// valid independently. Discarded bytes are not traced or dropped: clean up
    /// any resources there yourself before committing a shorter prefix.
    #[inline]
    pub unsafe fn commit(self, initialized_bytes: usize) -> Result<Pointer> {
        if initialized_bytes > self.max_bytes {
            return Err(Error::InvalidRange);
        }
        let extent = self.space.extent(initialized_bytes)?;
        // The exclusive borrow forbids cursor changes. Extent is monotonic in
        // byte length, so every accepted prefix fits the original reservation.
        // try_allocate writes the actual size prefix and advances only once.
        Ok(self
            .space
            .try_allocate(initialized_bytes, extent)
            .expect("reserved variable prefix fits"))
    }
}

impl RawHeap {
    pub fn new(config: Config, descriptors: &'static [Descriptor]) -> Result<Self> {
        Self::with_spare_chunks(config, descriptors, SpareChunkPool::new(&config))
    }

    /// A heap that takes its nursery chunks from, and returns them to,
    /// `spare`: the pool of the heaps whose nurseries are collected
    /// together.
    pub fn with_spare_chunks(
        config: Config,
        descriptors: &'static [Descriptor],
        spare: SpareChunkPool,
    ) -> Result<Self> {
        let mut spaces = Vec::new();
        spaces
            .try_reserve_exact(descriptors.len())
            .map_err(|_| Error::OutOfMemory)?;
        for descriptor in descriptors {
            spaces.push(Space::new(descriptor)?);
        }
        Ok(Self {
            descriptors,
            spaces,
            config,
            threshold: config.threshold_bytes,
            capacity: 0,
            old: OldHeap::new(descriptors.len(), config.chunk_bytes)?,
            major_threshold: config.threshold_bytes,
            major_growth: 2,
            stats: Stats::default(),
            scratch: Scratch::default(),
            spare,
            #[cfg(not(target_family = "wasm"))]
            parallel_tracing: false,
            #[cfg(not(target_family = "wasm"))]
            pool: None,
        })
    }

    /// Allow independent object trace callbacks to run concurrently on GC workers.
    /// Roots, weak updates and destructors still run on the collecting thread.
    /// No effect on wasm or when Config::workers is one.
    ///
    /// # Safety
    /// Every descriptor's trace callback must be safe on a worker thread and
    /// concurrent with callbacks for other objects. It may modify only its own
    /// payload, must not dereference managed outgoing edges, and must not access
    /// thread-affine or unsynchronized external state. The mutator stays stopped.
    pub unsafe fn enable_parallel_tracing(&mut self) {
        #[cfg(not(target_family = "wasm"))]
        {
            self.parallel_tracing = true;
        }
    }

    fn prepare_workers(&mut self, bytes: usize) -> Result<usize> {
        #[cfg(not(target_family = "wasm"))]
        if self.parallel_tracing
            && self.config.workers != 1
            && (self.config.workers != 0 || bytes >= 1024 * 1024)
        {
            if self.pool.is_none() {
                let workers = if self.config.workers == 0 {
                    std::thread::available_parallelism()
                        .map_or(1, usize::from)
                        .min(8)
                } else {
                    self.config.workers.min(64)
                };
                if workers > 1 {
                    self.pool = Some(pool::Pool::new(workers)?);
                }
            }
            return Ok(self.pool.as_ref().map_or(1, pool::Pool::workers));
        }
        let _ = bytes;
        Ok(1)
    }

    /// Physical allocation size, including any size prefix and alignment.
    pub fn allocation_extent(&self, kind: usize, bytes: usize) -> Result<usize> {
        self.spaces
            .get(kind)
            .ok_or(Error::InvalidRange)?
            .extent(bytes)
    }

    /// The bump fast path. Never collects or reserves a chunk. An invalid
    /// request also returns `None`; `allocate_slow` reports its error.
    ///
    /// # Safety
    /// Initialize the returned payload completely according to its descriptor
    /// before any further allocation, collection or destruction of the heap.
    /// Its managed edges must name initialized payloads in this same heap;
    /// external edges must remain valid independently. No payload may be pinned.
    #[inline]
    pub unsafe fn try_allocate(&mut self, kind: usize, bytes: usize) -> Option<Pointer> {
        let space = self.spaces.get_mut(kind)?;
        let extent = space.extent(bytes).ok()?;
        space.try_allocate(bytes, extent)
    }

    /// Bump a fixed-size, unpadded payload without inspecting its descriptor
    /// or range-checking its kind. Never collects or reserves a chunk;
    /// insufficient remaining space returns `None`.
    ///
    /// # Safety
    /// `kind` must identify an existing descriptor in this heap; integrations
    /// with a sealed descriptor table can establish this once when creating
    /// the heap. That descriptor must have `fixed_size == Some(BYTES)` with
    /// nonzero `BYTES` divisible by the descriptor's alignment. Thus its
    /// physical extent is exactly `BYTES` and it has no size prefix. These
    /// requirements are checked only in debug builds. The initialization and
    /// ownership requirements of [`Self::try_allocate`] also apply.
    #[inline]
    pub unsafe fn try_allocate_known_fixed<const BYTES: usize>(
        &mut self,
        kind: usize,
    ) -> Option<Pointer> {
        debug_assert!(kind < self.spaces.len());
        // SAFETY: the caller guarantees that the kind is in this heap's table.
        let space = unsafe { self.spaces.get_unchecked_mut(kind) };
        debug_assert_ne!(BYTES, 0);
        debug_assert_eq!(space.fixed_size, Some(BYTES));
        debug_assert_eq!(space.fixed_extent, Some(BYTES));
        debug_assert_eq!(space.prefix, 0);
        if BYTES > space.remaining {
            return None;
        }
        let pointer = space.cursor;
        // SAFETY: the caller guarantees the physical extent, and it fits the
        // remaining chunk. The new cursor may be its one-past-end pointer.
        space.cursor = unsafe { Pointer::new_unchecked(pointer.as_ptr().add(BYTES)) };
        space.remaining -= BYTES;
        Some(pointer)
    }

    /// Reserve two runs of independently traced fixed-size objects atomically.
    /// Chunk reservation may fail, but no object becomes visible until both
    /// spaces have enough capacity. Never collects.
    ///
    /// # Safety
    /// Both kinds must satisfy [`Self::try_allocate_known_fixed`], must be
    /// distinct, and both counts must be nonzero. Initialize EVERY object in
    /// BOTH returned spans before another heap allocation, collection, error
    /// return, unwind or heap destruction. Edges may refer between these two
    /// spans during initialization; all other edges must already be valid.
    #[inline]
    pub unsafe fn allocate_known_fixed_pair<const A_BYTES: usize, const B_BYTES: usize>(
        &mut self,
        kind_a: usize,
        count_a: usize,
        kind_b: usize,
        count_b: usize,
    ) -> Result<(Pointer, Pointer)> {
        debug_assert_ne!(kind_a, kind_b);
        debug_assert_ne!(count_a, 0);
        debug_assert_ne!(count_b, 0);
        debug_assert_ne!(A_BYTES, 0);
        debug_assert_ne!(B_BYTES, 0);
        debug_assert!(kind_a < self.spaces.len());
        debug_assert!(kind_b < self.spaces.len());
        let extent_a = A_BYTES
            .checked_mul(count_a)
            .ok_or(Error::AllocationTooLarge)?;
        let extent_b = B_BYTES
            .checked_mul(count_b)
            .ok_or(Error::AllocationTooLarge)?;
        // SAFETY: valid distinct kinds refer to separate spaces. All checks
        // precede either bump; these borrows end before a cold refill call.
        let spaces = self.spaces.as_mut_ptr();
        let a = unsafe { &mut *spaces.add(kind_a) };
        let b = unsafe { &mut *spaces.add(kind_b) };
        debug_assert_eq!(a.fixed_size, Some(A_BYTES));
        debug_assert_eq!(a.fixed_extent, Some(A_BYTES));
        debug_assert_eq!(a.prefix, 0);
        debug_assert_eq!(b.fixed_size, Some(B_BYTES));
        debug_assert_eq!(b.fixed_extent, Some(B_BYTES));
        debug_assert_eq!(b.prefix, 0);
        if extent_a > a.remaining || extent_b > b.remaining {
            return unsafe {
                self.allocate_known_fixed_pair_slow::<A_BYTES, B_BYTES>(
                    kind_a, count_a, extent_a, kind_b, count_b, extent_b,
                )
            };
        }
        let first = a.cursor;
        let second = b.cursor;
        // SAFETY: both complete spans fit. No fallible operation or callback
        // occurs between committing the first bump and returning both spans.
        unsafe {
            a.cursor = Pointer::new_unchecked(first.as_ptr().add(extent_a));
            b.cursor = Pointer::new_unchecked(second.as_ptr().add(extent_b));
        }
        a.remaining -= extent_a;
        b.remaining -= extent_b;
        Ok((first, second))
    }

    #[cold]
    unsafe fn allocate_known_fixed_pair_slow<const A_BYTES: usize, const B_BYTES: usize>(
        &mut self,
        kind_a: usize,
        count_a: usize,
        extent_a: usize,
        kind_b: usize,
        count_b: usize,
        extent_b: usize,
    ) -> Result<(Pointer, Pointer)> {
        // Refills expose no new objects. Cache EACH successful reservation,
        // including when the second one fails and leaves an empty first chunk.
        if self.spaces[kind_a].remaining < extent_a {
            let bytes = self.config.chunk_bytes.max(extent_a);
            self.spaces[kind_a].add_chunk(bytes, &mut self.spare.lock())?;
            self.capacity = self.capacity.saturating_add(bytes);
        }
        if self.spaces[kind_b].remaining < extent_b {
            let bytes = self.config.chunk_bytes.max(extent_b);
            self.spaces[kind_b].add_chunk(bytes, &mut self.spare.lock())?;
            self.capacity = self.capacity.saturating_add(bytes);
        }
        // SAFETY: descriptors were validated and both spaces now fit. This
        // commits both runs without another refill or collecting operation.
        unsafe {
            self.allocate_known_fixed_pair::<A_BYTES, B_BYTES>(kind_a, count_a, kind_b, count_b)
        }
    }

    /// Reserve a chunk if necessary and allocate, without collecting. The
    /// integration layer decides when to collect and supplies its roots.
    ///
    /// # Safety
    /// The same initialization and ownership requirements as `try_allocate`.
    #[cold]
    pub unsafe fn allocate_slow(&mut self, kind: usize, bytes: usize) -> Result<Pointer> {
        let extent = self.allocation_extent(kind, bytes)?;
        let space = &mut self.spaces[kind];
        if let Some(pointer) = space.try_allocate(bytes, extent) {
            return Ok(pointer);
        }
        let chunk_bytes = self.config.chunk_bytes.max(extent);
        space.add_chunk(chunk_bytes, &mut self.spare.lock())?;
        self.capacity = self.capacity.saturating_add(chunk_bytes);
        Ok(space
            .try_allocate(bytes, extent)
            .expect("new chunk fits request"))
    }

    /// Reserve capacity for an uncommitted variable-size payload. Never
    /// collects. The maximum only determines required capacity; commit advances
    /// by the actual initialized extent, leaving its unused tail reusable.
    ///
    /// Reserving is safe: the reservation borrows the heap, so nothing can
    /// allocate or collect until it is committed or dropped. The obligations
    /// lie with the raw writes through its payload pointer, which must stay
    /// within the maximum byte range and off the prefix, and with
    /// [`VariableReservation::commit`], which publishes them. Do not store
    /// pointers to the payload in roots or existing objects before commit. A
    /// dropped reservation is never observed or dropped by the collector, so
    /// the caller cleans up any resources it wrote there.
    #[inline]
    pub fn reserve_variable(
        &mut self,
        kind: usize,
        max_bytes: usize,
    ) -> Result<VariableReservation<'_>> {
        let space = self.spaces.get_mut(kind).ok_or(Error::InvalidRange)?;
        if space.fixed_size.is_some() {
            return Err(Error::InvalidRange);
        }
        let extent = space.extent(max_bytes)?;
        if extent > space.remaining {
            let bytes = self.config.chunk_bytes.max(extent);
            space.add_chunk(bytes, &mut self.spare.lock())?;
            self.capacity = self.capacity.saturating_add(bytes);
        }
        Ok(VariableReservation { space, max_bytes })
    }

    /// Threshold check for a pending physical extent. Normally called on
    /// refill, with `stress` checked before every allocation by the caller.
    /// The threshold is a trigger, not a hard memory limit.
    pub fn collection_due(&self, pending_extent: usize) -> bool {
        if self.config.stress {
            return true;
        }
        if self.capacity.saturating_add(pending_extent) <= self.threshold {
            return false;
        }
        let occupied = self.young_bytes();
        occupied != 0 && occupied.saturating_add(pending_extent) > self.threshold
    }

    pub fn used_bytes(&self) -> usize {
        self.young_bytes() + self.old.used
    }

    pub fn young_bytes(&self) -> usize {
        self.spaces.iter().map(Space::used).sum()
    }

    pub fn old_bytes(&self) -> usize {
        self.old.used
    }

    /// Record an object whose managed edges are about to change. Accepts its
    /// payload or an interior address; young and external owners are ignored.
    /// Does not allocate or collect. Call before every mutation that may install
    /// a managed pointer, even when the target currently appears to be old.
    #[inline]
    pub fn write_barrier(&mut self, owner: Pointer) {
        self.old.remember(owner, self.descriptors);
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity + self.old.capacity
    }

    /// Reserved nursery backing, for aggregate multi-mutator pressure checks.
    pub fn nursery_capacity_bytes(&self) -> usize {
        self.capacity
    }

    /// Descriptor name, occupied bytes and reserved chunk bytes for each kind.
    /// Occupied bytes include garbage pending collection and layout padding.
    /// Reads nursery totals and old-slot metadata without tracing payloads.
    /// Occupancy uses the nursery layout size; old size-class slack is included
    /// in reserved capacity, not occupied bytes.
    pub fn space_usage(&self) -> Vec<(&'static str, usize, usize)> {
        self.descriptors
            .iter()
            .zip(&self.spaces)
            .enumerate()
            .map(|(kind, (descriptor, space))| {
                let (used, capacity) = self.old.usage(kind, space, self.descriptors);
                (
                    descriptor.name,
                    space.used() + used,
                    space.capacity + capacity,
                )
            })
            .collect()
    }

    /// Collection counters; timing is measured only during collection and is
    /// zero on wasm, where no clock support is required.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Transfer another mutator's nursery into this heap without moving any
    /// payload. The donor may allocate a fresh nursery afterwards. This heap
    /// owns the common old generation; donors must never collect independently.
    ///
    /// # Safety
    /// Both heaps must belong to the same externally branded graph and all
    /// mutators must be stopped. Their descriptor arrays must be identical.
    /// The eventual collection must trace the union of every mutator's roots
    /// and replay every mutator's old-object write barriers. No uncommitted
    /// variable reservation may exist. On error no payload is moved or freed.
    pub unsafe fn absorb_nursery(&mut self, donor: &mut RawHeap) -> Result<()> {
        if !std::ptr::eq(self.descriptors, donor.descriptors) || donor.old.used != 0 {
            return Err(Error::InvalidRange);
        }
        // Reserve before changing any ownership or object-boundary indices.
        let capacity = self
            .capacity
            .checked_add(donor.capacity)
            .ok_or(Error::AllocationTooLarge)?;
        for (destination, source) in self.spaces.iter_mut().zip(&donor.spaces) {
            destination
                .chunks
                .try_reserve(source.chunks.len())
                .map_err(|_| Error::OutOfMemory)?;
            destination
                .capacity
                .checked_add(source.capacity)
                .ok_or(Error::AllocationTooLarge)?;
            destination
                .variable_count
                .checked_add(source.variable_count)
                .ok_or(Error::AllocationTooLarge)?;
        }
        for (destination, source) in self.spaces.iter_mut().zip(&mut donor.spaces) {
            if source.chunks.is_empty() {
                continue;
            }
            let active_used = destination.active_capacity - destination.remaining;
            if let Some(last) = destination.chunks.last_mut() {
                last.used = active_used;
            }
            let variables = destination.variable_count;
            for chunk in &mut source.chunks {
                chunk.first_variable += variables;
            }
            destination.retired_used += active_used + source.retired_used;
            destination.variable_count += source.variable_count;
            destination.capacity += source.capacity;
            destination.cursor = source.cursor;
            destination.remaining = source.remaining;
            destination.active_capacity = source.active_capacity;
            destination.chunks.append(&mut source.chunks);
            source.cursor = Pointer::dangling();
            source.remaining = 0;
            source.active_capacity = 0;
            source.retired_used = 0;
            source.variable_count = 0;
            source.capacity = 0;
        }
        self.capacity = capacity;
        donor.capacity = 0;
        Ok(())
    }

    /// Collect both generations: promote reachable young objects and mark and
    /// compact old objects, repairing weak references before dropping garbage.
    /// All fallible reservations finish before `trace_roots` runs. Failure may
    /// retain empty reserved slabs, but never changes roots or payloads. Promotion
    /// reserves enough old slots for every nursery object before tracing.
    ///
    /// # Safety
    /// No references or unregistered raw pointers may be used across this call.
    /// Enumerate each strong root slot exactly once per invocation of
    /// `trace_roots`. A major collection invokes it twice: marking/promotion,
    /// then pointer repair. Do not dereference edges after rewriting them in
    /// callbacks: compaction destinations are not initialized until repair ends.
    /// Descriptors must enumerate every managed edge and satisfy their contract.
    /// External pointers must be independent of this heap and remain valid.
    /// Only payload addresses or in-bounds interior addresses are supported:
    /// canonicalize empty slices instead of retaining one-past-end pointers.
    /// `weak_update` must rewrite surviving weak references and remove dead
    /// ones, without promoting objects. Neither callback may unwind or reenter.
    pub unsafe fn collect(
        &mut self,
        trace_roots: impl FnMut(&mut Tracer<'_>),
        weak_update: impl FnOnce(&Relocation<'_>),
    ) -> Result<()> {
        // SAFETY: caller supplies exact roots and suspends all views.
        unsafe { self.collect_generation(true, true, trace_roots, weak_update) }
    }

    /// Whether [`RawHeap::collect_minor`], called now, would be a major
    /// collection: weak-reference repair differs (see [`Relocation`]).
    pub fn minor_collects_old(&self) -> bool {
        self.old.used >= self.major_threshold
    }

    /// Collect the nursery, upgrading to a major collection when the old
    /// generation reaches its budget. Intended for allocation safepoints.
    ///
    /// # Safety
    /// The `collect` contract applies. Every old-object edge mutation since
    /// promotion must have been recorded with `write_barrier`.
    pub unsafe fn collect_minor(
        &mut self,
        trace_roots: impl FnMut(&mut Tracer<'_>),
        weak_update: impl FnOnce(&Relocation<'_>),
    ) -> Result<()> {
        let major = self.minor_collects_old();
        // SAFETY: forwarded caller contract.
        unsafe { self.collect_generation(major, false, trace_roots, weak_update) }
    }

    unsafe fn collect_generation(
        &mut self,
        major: bool,
        force_compaction: bool,
        mut trace_roots: impl FnMut(&mut Tracer<'_>),
        weak_update: impl FnOnce(&Relocation<'_>),
    ) -> Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let started = std::time::Instant::now();
        #[cfg(not(target_family = "wasm"))]
        let mut phase_at = started;
        #[cfg(not(target_family = "wasm"))]
        let mut phases = [std::time::Duration::ZERO; 8];
        macro_rules! phase {
            ($index:expr) => {
                #[cfg(not(target_family = "wasm"))]
                if self.config.trace_collections {
                    let now = std::time::Instant::now();
                    phases[$index] = now.duration_since(phase_at);
                    phase_at = now;
                }
            };
        }
        #[cfg(feature = "trace-counts")]
        let mut trace_counts = TraceCounts {
            kinds: reserved(self.descriptors.len())?,
            ..TraceCounts::default()
        };
        #[cfg(feature = "trace-counts")]
        trace_counts.kinds.resize(self.descriptors.len(), [0; 2]);
        let before = self.used_bytes();
        let workers = self.prepare_workers(if major { before } else { self.young_bytes() })?;
        if self.config.trace_collections {
            eprintln!("gc start: cycle={} kind={} young={} old={} capacity={} nursery_budget={} major_budget={} workers={}",
                self.stats.collections + 1, if major { "major" } else { "minor" },
                self.young_bytes(), self.old.used, self.capacity_bytes(), self.threshold, self.major_threshold, workers);
        }
        let mut count = 0usize;
        let mut variable_count = 0usize;
        let mut small_count = 0usize;
        for space in &self.spaces {
            if let Some(extent) = space.fixed_extent {
                count += space.used() / extent;
                if extent < size_of::<Pointer>() {
                    small_count += space.used() / extent;
                }
            } else {
                variable_count += space.variable_count;
            }
        }
        count += variable_count;
        let mut scratch = std::mem::take(&mut self.scratch);
        let mut forwarded = Forwarding::new(count, small_count, &mut scratch)?;
        let mut variables = scratch::take(&mut scratch.variables, variable_count)?;
        let mut ranges = scratch::take(
            &mut scratch.ranges,
            self.spaces.iter().map(|space| space.chunks.len()).sum(),
        )?;
        #[cfg(not(target_family = "wasm"))]
        let mut claimed = scratch::take(&mut scratch.claimed, if workers > 1 { count } else { 0 })?;
        #[cfg(not(target_family = "wasm"))]
        if workers > 1 {
            claimed.resize_with(count, || std::sync::atomic::AtomicU8::new(0));
        }
        let extra_work = if major { self.old.count } else { 0 };
        #[cfg(not(target_family = "wasm"))]
        let extra_work = if workers > 1 && !major {
            self.old.dirty_count()
        } else {
            extra_work
        };
        let mut pending = Worklist::new(count + extra_work)?;
        let mut demands = self.old.demands()?;
        let mut first = 0;
        let mut variable_offset = 0;
        let mut small_first = 0;
        for (kind, source) in self.spaces.iter().enumerate() {
            if let Some(bytes) = source.fixed_size {
                let (bin, _) = OldHeap::bin(kind, source, bytes)?;
                demands[bin] += source.used() / source.fixed_extent.unwrap();
            }
            for (index, chunk) in source.chunks.iter().enumerate() {
                let used = if index + 1 == source.chunks.len() {
                    source.active_capacity - source.remaining
                } else {
                    chunk.used
                };
                if used == 0 {
                    continue;
                }
                let variable_first = variable_offset;
                let count = if let Some(extent) = source.fixed_extent {
                    used / extent
                } else if workers > 1 {
                    source
                        .chunks
                        .get(index + 1)
                        .map_or(source.variable_count, |next| next.first_variable)
                        - chunk.first_variable
                } else {
                    let mut offset = 0;
                    while offset < used {
                        // SAFETY: every variable allocation has an initialized
                        // size prefix. No root has been changed at this point.
                        let bytes =
                            unsafe { chunk.pointer.as_ptr().add(offset).cast::<usize>().read() };
                        // Validate promotion classes before roots can change.
                        demands[OldHeap::bin(kind, source, bytes)?.0] += 1;
                        variables.push(VariableObject {
                            offset: offset + source.prefix,
                            bytes,
                        });
                        offset += source.extent_known(bytes);
                    }
                    debug_assert_eq!(offset, used);
                    variables.len() - variable_first
                };
                if source.fixed_extent.is_none() {
                    variable_offset += count;
                }
                ranges.push(ChunkRange {
                    reciprocal: source
                        .fixed_extent
                        .map_or(0, |extent| slot_reciprocal(extent, used)),
                    pointer: chunk.pointer,
                    end: chunk.pointer.as_ptr() as usize + used,
                    first,
                    count,
                    kind,
                    fixed_size: source.fixed_size,
                    fixed_extent: source.fixed_extent,
                    variable_first,
                    variable_block_first: 0,
                    small_first: if source
                        .fixed_extent
                        .is_some_and(|extent| extent < size_of::<Pointer>())
                    {
                        let first = small_first;
                        small_first += count;
                        first
                    } else {
                        usize::MAX
                    },
                });
                first += count;
            }
        }
        debug_assert_eq!(first, forwarded.objects);
        debug_assert_eq!(small_first, small_count);
        debug_assert_eq!(variable_offset, variable_count);
        #[cfg(not(target_family = "wasm"))]
        if workers > 1 {
            parallel::prepare_variables(
                &mut variables,
                variable_count,
                &ranges,
                &self.spaces,
                &mut demands,
                self.pool.as_mut().unwrap(),
            )?;
        }
        let range_index = RangeIndex::build(
            &mut ranges,
            &variables,
            &mut scratch,
            #[cfg(not(target_family = "wasm"))]
            if workers > 1 {
                self.pool.as_mut()
            } else {
                None
            },
        )?;
        phase!(0);
        #[cfg(not(target_family = "wasm"))]
        if workers > 1 {
            // At most one partial promotion lease per worker and size class.
            // Reserve this slack even if every nursery object survives.
            OldHeap::worker_slack(&mut demands, &self.spaces, self.config.chunk_bytes, workers)?;
        }
        #[cfg(not(target_family = "wasm"))]
        if workers > 1 {
            self.old.reserve_parallel(
                &demands,
                &self.spaces,
                self.config.chunk_bytes,
                self.pool.as_mut().unwrap(),
            )?;
        }
        if workers == 1 {
            self.old
                .reserve(&demands, &self.spaces, self.config.chunk_bytes)?;
        }
        let mut compaction = if major {
            Some(self.old.reserve_compaction()?)
        } else {
            None
        };
        #[cfg(not(target_family = "wasm"))]
        let mut maintenance = if major && workers > 1 {
            Some(old::Maintenance::reserve(&self.old)?)
        } else {
            None
        };
        let peak = self
            .capacity_bytes()
            .saturating_add(compaction.as_ref().map_or(0, Compaction::capacity))
            .saturating_add(self.old.metadata_capacity())
            .saturating_add(forwarded.capacity_bytes())
            .saturating_add(variables.capacity() * size_of::<VariableObject>())
            .saturating_add(ranges.capacity() * size_of::<ChunkRange>())
            .saturating_add(range_index.capacity_bytes())
            .saturating_add(pending.capacity_bytes())
            .saturating_add(demands.capacity() * size_of::<usize>());
        let old_before = self.old.used;
        #[cfg(not(target_family = "wasm"))]
        let mut peak = peak;
        #[cfg(not(target_family = "wasm"))]
        {
            peak = peak.saturating_add(claimed.capacity());
        }
        #[cfg(not(target_family = "wasm"))]
        if let Some(maintenance) = &maintenance {
            peak = peak.saturating_add(maintenance.capacity_bytes());
        }
        #[cfg(not(target_family = "wasm"))]
        if workers > 1 {
            let mut graph = parallel::Graph::new(
                &mut self.old,
                &mut forwarded,
                &variables,
                &ranges,
                &range_index,
                &self.spaces,
                self.descriptors,
                major,
                &mut pending,
                workers,
                &claimed,
            )?;
            peak = peak.saturating_add(graph.capacity_bytes());
            phase!(1);
            graph.roots(&mut trace_roots);
            phase!(2);
            graph.remembered();
            phase!(3);
            graph.trace(self.pool.as_mut().expect("parallel pool prepared"));
            graph.finish(
                #[cfg(feature = "trace-counts")]
                &mut trace_counts,
            );
        }
        // No fallible operation from here until source payloads are retired.
        if workers == 1 {
            phase!(1);
            let mut tracer = Tracer {
                inner: TracerKind::Serial(SerialTracer {
                    #[cfg(feature = "trace-counts")]
                    counts: &mut trace_counts,
                    forwarded: &mut forwarded,
                    variables: &variables,
                    ranges: &ranges,
                    range_index: &range_index,
                    spaces: &self.spaces,
                    old: &mut self.old,
                    descriptors: self.descriptors,
                    major,
                    compaction: None,
                    pending: &mut pending,
                }),
            };
            trace_roots(&mut tracer);
            phase!(2);
            if !major {
                while let Some(page) = tracer.serial().old.take_dirty_page() {
                    for slot in 0..tracer.serial().old.slots(page) {
                        if let Some((pointer, bytes, kind)) =
                            tracer.serial().old.take_dirty(page, slot, self.descriptors)
                        {
                            // SAFETY: remembered slots are initialized old owners.
                            unsafe { (self.descriptors[kind].trace)(pointer, bytes, &mut tracer) };
                        }
                    }
                }
            }
            phase!(3);
            // Every object is queued at most once: on promotion or first mark.
            // A minor collection queues no pre-existing old object.
            while let Some(work) = tracer.serial().pending.pop() {
                let (pointer, bytes, kind) = tracer.serial().old.trace_info(work, self.descriptors);
                // SAFETY: queued payloads were initialized before publishing.
                unsafe { (self.descriptors[kind].trace)(pointer, bytes, &mut tracer) };
            }
        }
        phase!(4);
        #[cfg(not(target_family = "wasm"))]
        if let Some(maintenance) = &mut maintenance {
            maintenance.prepare(&mut self.old, &self.spaces);
        }
        // Dense old heaps gain little from moving almost every survivor.
        // Reuse their dead slots directly; explicit full collections still
        // compact, and automatic majors compact once fragmentation warrants it.
        if major && !force_compaction {
            #[cfg(not(target_family = "wasm"))]
            let should_compact = if let Some(maintenance) = &maintenance {
                maintenance.should_compact(self.pool.as_mut().unwrap())
            } else {
                self.old.should_compact()
            };
            #[cfg(target_family = "wasm")]
            let should_compact = self.old.should_compact();
            if !should_compact {
                compaction = None;
            }
        }
        if let Some(plan) = &mut compaction {
            #[cfg(not(target_family = "wasm"))]
            if let Some(maintenance) = &maintenance {
                let pool = self.pool.as_mut().unwrap();
                maintenance.plan(plan, pool);
                parallel::repair(
                    &self.old,
                    self.descriptors,
                    plan,
                    pool,
                    &mut trace_roots,
                    #[cfg(feature = "trace-counts")]
                    &mut trace_counts,
                );
            }
            if workers == 1 {
                self.old.plan_compaction(plan);
                let mut tracer = Tracer {
                    inner: TracerKind::Serial(SerialTracer {
                        #[cfg(feature = "trace-counts")]
                        counts: &mut trace_counts,
                        forwarded: &mut forwarded,
                        variables: &variables,
                        ranges: &ranges,
                        range_index: &range_index,
                        spaces: &self.spaces,
                        old: &mut self.old,
                        descriptors: self.descriptors,
                        major,
                        compaction: Some(plan),
                        pending: &mut pending,
                    }),
                };
                trace_roots(&mut tracer);
                for page in 0..tracer.serial().old.pages() {
                    for slot in 0..tracer.serial().old.slots(page) {
                        if let Some((pointer, bytes, kind)) =
                            tracer
                                .serial()
                                .old
                                .marked_object(page, slot, self.descriptors)
                        {
                            // SAFETY: rewrite each live source's edges once before
                            // moving any payload to its planned destination.
                            unsafe { (self.descriptors[kind].trace)(pointer, bytes, &mut tracer) };
                        }
                    }
                }
            }
        }
        phase!(5);
        weak_update(&Relocation {
            forwarded: &forwarded,
            variables: &variables,
            ranges: &ranges,
            range_index: &range_index,
            old: &self.old,
            descriptors: self.descriptors,
            major,
            compaction: compaction.as_ref(),
        });
        for range in &ranges {
            if let Some(drop) = self.descriptors[range.kind].drop {
                for local in 0..range.count {
                    if !forwarded.contains(range.first + local) {
                        let (source, bytes) = range.object(&variables, local);
                        // SAFETY: only dead source payloads still own resources;
                        // forwarded originals must never run their destructor.
                        unsafe { drop(source, bytes) };
                    }
                }
            }
        }
        phase!(6);
        let promoted = self.old.used - old_before;
        if major {
            #[cfg(not(target_family = "wasm"))]
            let moved = if let Some(maintenance) = &mut maintenance {
                let pool = self.pool.as_mut().unwrap();
                if let Some(plan) = &compaction {
                    maintenance.compact(&mut self.old, self.descriptors, plan, pool)
                } else {
                    maintenance.sweep(&mut self.old, self.descriptors, pool);
                    0
                }
            } else if let Some(plan) = &compaction {
                self.old.compact(self.descriptors, &self.spaces, plan)
            } else {
                self.old.sweep(self.descriptors, &self.spaces);
                0
            };
            #[cfg(target_family = "wasm")]
            let moved = if let Some(plan) = &compaction {
                self.old.compact(self.descriptors, &self.spaces, plan)
            } else {
                self.old.sweep(self.descriptors, &self.spaces);
                0
            };
            self.stats.compacted_bytes += moved;
            self.stats.copied_bytes += moved;
            // Mark/compact is poor value when nearly the entire old graph
            // survives. Back off geometrically, as the copying collector did,
            // while keeping the nursery budget independent and fixed.
            let examined = old_before + promoted;
            let reclaimed = examined - self.old.used;
            self.stats.old_reclaimed_bytes += reclaimed;
            self.major_growth = if examined != 0 && reclaimed <= examined / 4 {
                (self.major_growth * 2).min(8)
            } else {
                2
            };
            self.major_threshold = self
                .config
                .threshold_bytes
                .max(self.old.used.saturating_mul(self.major_growth));
            // The startup allowance also bounds how much old garbage may
            // accumulate between major cycles in the two-stage policy.
            // Multiplicative backoff alone can defer reclamation until the
            // old heap, promotion space and scratch exhaust physical memory.
            if self.config.steady_threshold_bytes.is_some() {
                self.major_threshold = self
                    .major_threshold
                    .min(self.old.used.saturating_add(self.config.threshold_bytes));
            }
            self.stats.major_collections += 1;
        } else {
            self.old.release_empty_pages();
            self.stats.minor_collections += 1;
        }
        self.threshold = self
            .config
            .steady_threshold_bytes
            .unwrap_or(self.config.threshold_bytes);
        let mut spare = self.spare.lock();
        for (kind, space) in self.spaces.iter_mut().enumerate() {
            let emptied = std::mem::replace(
                space,
                Space::new(&self.descriptors[kind]).expect("validated descriptor"),
            );
            for chunk in emptied.chunks {
                spare.keep(chunk, self.threshold);
            }
        }
        drop(spare);
        self.capacity = 0;
        let live = self.old.used;
        self.stats.collections += 1;
        self.stats.last_collection_workers = workers;
        self.stats.parallel_collections += usize::from(workers > 1);
        self.stats.copied_bytes += promoted;
        self.stats.promoted_bytes += promoted;
        self.stats.reclaimed_bytes += before - live;
        self.stats.last_live_bytes = live;
        self.stats.peak_collection_capacity = self.stats.peak_collection_capacity.max(peak);
        // Every worker and relocation view has finished using these buffers.
        // Only capacities survive; the next cycle rebuilds all metadata.
        scratch.bits = forwarded.bits;
        scratch.small = forwarded.small;
        scratch.variables = variables;
        scratch.ranges = ranges;
        scratch.slots = range_index.slots;
        scratch.blocks = range_index.blocks;
        #[cfg(not(target_family = "wasm"))]
        {
            scratch.claimed = claimed;
        }
        scratch.retain();
        self.scratch = scratch;
        #[cfg(not(target_family = "wasm"))]
        {
            let elapsed = started.elapsed();
            self.stats.collection_time += elapsed;
            if major {
                self.stats.major_collection_time += elapsed;
            } else {
                self.stats.minor_collection_time += elapsed;
            }
        }
        if self.config.trace_collections {
            #[cfg(feature = "trace-counts")]
            eprintln!(
                "gc trace counts: visits={} young={} forwarded={} promoted={} old={} old_marked={}",
                trace_counts.visits,
                trace_counts.young,
                trace_counts.forwarded,
                trace_counts.promoted,
                trace_counts.old,
                trace_counts.old_marked
            );
            #[cfg(feature = "trace-counts")]
            for (kind, &[promoted, forwarded]) in trace_counts.kinds.iter().enumerate() {
                if promoted != 0 || forwarded != 0 {
                    eprintln!(
                        "gc trace kind: kind={} name={:?} promoted={} forwarded={}",
                        kind, self.descriptors[kind].name, promoted, forwarded
                    );
                }
            }
            #[cfg(not(target_family = "wasm"))]
            {
                phases[7] = phase_at.elapsed();
                let ms = phases.map(|d| d.as_secs_f64() * 1000.0);
                eprintln!("gc phases ms: index={:.3} reserve={:.3} roots={:.3} remembered={:.3} graph={:.3} plan={:.3} weak_drop={:.3} reclaim={:.3}", ms[0], ms[1], ms[2], ms[3], ms[4], ms[5], ms[6], ms[7]);
            }
            eprintln!(
                "gc end: cycle={} live={} capacity={} promoted={} reclaimed={} cumulative_ms={:.3}",
                self.stats.collections,
                live,
                self.capacity_bytes(),
                promoted,
                before - live,
                self.stats.collection_time.as_secs_f64() * 1000.0
            );
        }
        Ok(())
    }
}

impl Drop for RawHeap {
    fn drop(&mut self) {
        self.old.drop_objects(self.descriptors);
        for (kind, space) in self.spaces.iter().enumerate() {
            if let Some(drop) = self.descriptors[kind].drop {
                space.objects(|pointer, bytes, _| {
                    // SAFETY: every payload remaining in the heap owns its
                    // resources; forwarded source chunks were already retired.
                    unsafe { drop(pointer, bytes) };
                });
            }
        }
    }
}

/// Source chunk metadata is shared by all its fixed-size objects. Only
/// variable allocations need per-object boundaries, separate from forwarding.
struct VariableObject {
    offset: usize,
    bytes: usize,
}

// For n < bytes and bytes * stride < 2^32, ceil(2^32 / stride)
// gives exact floor division with one multiply and shift. The rounding error
// is below 1/stride, so it cannot cross the next integer quotient. Larger
// ranges retain ordinary division. This also handles a stride of one.
fn slot_reciprocal(stride: usize, bytes: usize) -> u64 {
    if stride
        .checked_mul(bytes)
        .is_some_and(|bound| (bound as u64) < (1u64 << 32))
    {
        (1u64 << 32).div_ceil(stride as u64)
    } else {
        0
    }
}

#[inline(always)]
fn slot_index(offset: usize, stride: usize, reciprocal: u64) -> usize {
    if reciprocal != 0 {
        ((offset as u64 * reciprocal) >> 32) as usize
    } else {
        offset / stride
    }
}

struct ChunkRange {
    reciprocal: u64,
    pointer: Pointer,
    end: usize,
    first: usize,
    count: usize,
    kind: usize,
    fixed_size: Option<usize>,
    fixed_extent: Option<usize>,
    variable_first: usize,
    variable_block_first: usize,
    small_first: usize,
}

impl ChunkRange {
    fn object(&self, variables: &[VariableObject], local: usize) -> (Pointer, usize) {
        let (offset, bytes) = match (self.fixed_extent, self.fixed_size) {
            (Some(extent), Some(bytes)) => (local * extent, bytes),
            _ => {
                let object = &variables[self.variable_first + local];
                (object.offset, object.bytes)
            }
        };
        // SAFETY: the index denotes an initialized allocation in this chunk.
        (
            unsafe { Pointer::new_unchecked(self.pointer.as_ptr().add(offset)) },
            bytes,
        )
    }
}

struct Located {
    index: usize,
    small: usize,
    offset: usize,
    source: Pointer,
    bytes: usize,
    kind: usize,
}

/// A compact address index avoids a binary search through nursery chunks on
/// every traced edge, including the many edges that already point into old
/// storage. Multiple unaligned chunks may share an address granule.
struct RangeIndex {
    slots: Vec<usize>,
    blocks: Vec<VariableBlock>,
}

// One bit per word-aligned payload start, with a prefix count per 64 words.
// Rank finds the owning variable object in constant time, including interiors
// arbitrarily far into a large allocation. This costs 16 bytes per 512 bytes
// of variable nursery storage on a 64-bit host.
struct VariableBlock {
    starts: u64,
    preceding: usize,
}

impl RangeIndex {
    const SHIFT: u32 = 16;

    fn bucket(address: usize, mask: usize) -> usize {
        let key = address >> Self::SHIFT;
        {
            let mixed = (key ^ (key >> 16)).wrapping_mul(0x9e3779b9);
            // Fold after multiplication too: low-bit masking alone aliases
            // chunks separated by 1 GiB in a small nursery address table.
            (mixed ^ (mixed >> (usize::BITS / 2))) & mask
        }
    }

    #[cfg(test)]
    fn new(ranges: &mut [ChunkRange], variables: &[VariableObject]) -> Result<Self> {
        Self::build(
            ranges,
            variables,
            &mut Scratch::default(),
            #[cfg(not(target_family = "wasm"))]
            None,
        )
    }

    fn build(
        ranges: &mut [ChunkRange],
        variables: &[VariableObject],
        scratch: &mut Scratch,
        #[cfg(not(target_family = "wasm"))] pool: Option<&mut pool::Pool>,
    ) -> Result<Self> {
        #[cfg(not(target_family = "wasm"))]
        let parallel = pool.is_some();
        #[cfg(target_family = "wasm")]
        let parallel = false;
        let entries: usize = ranges
            .iter()
            .map(|range| {
                ((range.end - 1) >> Self::SHIFT) - (range.pointer.as_ptr() as usize >> Self::SHIFT)
                    + 1
            })
            .sum();
        let capacity = entries
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .and_then(usize::checked_next_power_of_two)
            .ok_or(Error::AllocationTooLarge)?;
        let mut slots = scratch::take(&mut scratch.slots, capacity)?;
        slots.resize(capacity, usize::MAX);
        for (index, range) in ranges.iter().enumerate() {
            let first = range.pointer.as_ptr() as usize >> Self::SHIFT;
            let last = (range.end - 1) >> Self::SHIFT;
            for granule in first..=last {
                let mut slot = Self::bucket(granule << Self::SHIFT, capacity - 1);
                while slots[slot] != usize::MAX {
                    slot = (slot + 1) & (capacity - 1);
                }
                slots[slot] = index;
            }
        }
        let block_count = ranges
            .iter()
            .filter(|range| range.fixed_extent.is_none())
            .map(|range| {
                (range.end - range.pointer.as_ptr() as usize).div_ceil(64 * align_of::<usize>())
            })
            .sum();
        let mut blocks = scratch::take(&mut scratch.blocks, block_count)?;
        for range in ranges
            .iter_mut()
            .filter(|range| range.fixed_extent.is_none())
        {
            range.variable_block_first = blocks.len();
            let count =
                (range.end - range.pointer.as_ptr() as usize).div_ceil(64 * align_of::<usize>());
            blocks.resize_with(blocks.len() + count, || VariableBlock {
                starts: 0,
                preceding: 0,
            });
            if parallel {
                continue;
            }
            let local = &mut blocks[range.variable_block_first..];
            for object in &variables[range.variable_first..range.variable_first + range.count] {
                debug_assert_eq!(object.offset % align_of::<usize>(), 0);
                let word = object.offset / align_of::<usize>();
                local[word / 64].starts |= 1u64 << (word % 64);
            }
            let mut preceding = 0;
            for block in local {
                block.preceding = preceding;
                preceding += block.starts.count_ones() as usize;
            }
            debug_assert_eq!(preceding, range.count);
        }
        #[cfg(not(target_family = "wasm"))]
        if let Some(pool) = pool {
            parallel::index_blocks(&mut blocks, ranges, variables, pool);
        }
        Ok(Self { slots, blocks })
    }

    fn capacity_bytes(&self) -> usize {
        self.slots.capacity() * size_of::<usize>()
            + self.blocks.capacity() * size_of::<VariableBlock>()
    }

    #[inline(always)]
    fn count_starts(&self, bits: u64) -> usize {
        // One POPCNT instruction: x86-64 builds require the feature (lib.rs).
        bits.count_ones() as usize
    }

    #[inline]
    fn variable_owner(&self, range: &ChunkRange, relative: usize) -> Option<usize> {
        let word = relative / align_of::<usize>();
        let block = &self.blocks[range.variable_block_first + word / 64];
        let mask = u64::MAX >> (63 - word % 64);
        (block.preceding + self.count_starts(block.starts & mask)).checked_sub(1)
    }

    #[inline]
    fn find<'a>(&self, ranges: &'a [ChunkRange], address: usize) -> Option<&'a ChunkRange> {
        let mask = self.slots.len() - 1;
        let mut slot = Self::bucket(address, mask);
        loop {
            let index = self.slots[slot];
            if index == usize::MAX {
                return None;
            }
            let range = &ranges[index];
            if address >= range.pointer.as_ptr() as usize && address < range.end {
                return Some(range);
            }
            slot = (slot + 1) & mask;
        }
    }
}

#[inline(always)]
fn find(
    variables: &[VariableObject],
    ranges: &[ChunkRange],
    index: &RangeIndex,
    pointer: Pointer,
) -> Option<Located> {
    let address = pointer.as_ptr() as usize;
    let range = index.find(ranges, address)?;
    let relative = address - range.pointer.as_ptr() as usize;
    let local = if let Some(extent) = range.fixed_extent {
        slot_index(relative, extent, range.reciprocal)
    } else {
        index.variable_owner(range, relative)?
    };
    let (source, bytes) = range.object(variables, local);
    let offset = address - source.as_ptr() as usize;
    (offset < bytes.max(1)).then_some(Located {
        index: range.first + local,
        small: if range.small_first == usize::MAX {
            usize::MAX
        } else {
            range.small_first + local
        },
        offset,
        source,
        bytes,
        kind: range.kind,
    })
}

/// Once a nursery payload has been copied, its first word can hold the
/// forwarding address. A bit per object distinguishes these retired payloads
/// from live source data. Only fixed allocations smaller than a word need a
/// separate pointer table; descriptors and dead-object drops never read a
/// retired payload. Variable size prefixes are left intact.
struct Forwarding {
    objects: usize,
    bits: Vec<usize>,
    small: Vec<Option<Pointer>>,
}

impl Forwarding {
    fn new(objects: usize, small_count: usize, scratch: &mut Scratch) -> Result<Self> {
        let words = objects.div_ceil(usize::BITS as usize);
        let mut bits = scratch::take(&mut scratch.bits, words)?;
        bits.resize(words, 0);
        let mut small = scratch::take(&mut scratch.small, small_count)?;
        small.resize(small_count, None);
        Ok(Self {
            objects,
            bits,
            small,
        })
    }

    #[inline]
    fn contains(&self, index: usize) -> bool {
        self.bits[index / usize::BITS as usize] & (1 << (index % usize::BITS as usize)) != 0
    }

    #[inline]
    fn get(&self, object: &Located) -> Option<Pointer> {
        if !self.contains(object.index) {
            return None;
        }
        if object.small == usize::MAX {
            // SAFETY: the set bit means set() replaced this retired source's
            // first word with its valid forwarding pointer.
            Some(unsafe { object.source.cast::<Pointer>().as_ptr().read_unaligned() })
        } else {
            self.small[object.small]
        }
    }

    fn set(&mut self, object: &Located, target: Pointer) {
        if object.small == usize::MAX {
            // SAFETY: collection has copied the complete initialized payload.
            // The original allocation extent fits a pointer, even if some of
            // that word is padding. No mutator view is live during collection.
            unsafe {
                object
                    .source
                    .cast::<Pointer>()
                    .as_ptr()
                    .write_unaligned(target);
            }
        } else {
            self.small[object.small] = Some(target);
        }
        self.bits[object.index / usize::BITS as usize] |=
            1 << (object.index % usize::BITS as usize);
    }

    fn capacity_bytes(&self) -> usize {
        self.bits.capacity() * size_of::<usize>()
            + self.small.capacity() * size_of::<Option<Pointer>>()
    }
}

/// Reserve the worst-case worklist without asking a pre-faulting allocator to
/// touch hundreds of megabytes that a depth-first traversal may never use.
/// Small blocks stay uninitialized until pushed; all reservations still finish
/// before roots change, so tracing itself remains allocation-free.
struct Worklist {
    blocks: Vec<Vec<usize>>,
    len: usize,
    capacity: usize,
}

impl Worklist {
    const BLOCK: usize = 32 * 1024;

    fn new(capacity: usize) -> Result<Self> {
        let mut blocks = reserved(capacity.div_ceil(Self::BLOCK))?;
        let mut remaining = capacity;
        while remaining != 0 {
            let entries = remaining.min(Self::BLOCK);
            blocks.push(reserved(entries)?);
            remaining -= entries;
        }
        Ok(Self {
            blocks,
            len: 0,
            capacity,
        })
    }

    #[inline]
    fn push(&mut self, work: usize) {
        assert!(self.len < self.capacity, "trace worklist bound");
        let block = &mut self.blocks[self.len / Self::BLOCK];
        // SAFETY: every block was reserved in full before tracing. usize is
        // Copy; the Vec length stays zero and its allocation owns no resources.
        unsafe {
            block.as_mut_ptr().add(self.len % Self::BLOCK).write(work);
        }
        self.len += 1;
    }

    #[inline]
    fn pop(&mut self) -> Option<usize> {
        self.len = self.len.checked_sub(1)?;
        // SAFETY: this entry was initialized by push and has not been popped.
        Some(unsafe {
            self.blocks[self.len / Self::BLOCK]
                .as_ptr()
                .add(self.len % Self::BLOCK)
                .read()
        })
    }

    fn capacity_bytes(&self) -> usize {
        self.blocks.capacity() * size_of::<Vec<usize>>()
            + self
                .blocks
                .iter()
                .map(|block| block.capacity() * size_of::<usize>())
                .sum::<usize>()
    }
}

#[cfg(feature = "trace-counts")]
#[derive(Default)]
struct TraceCounts {
    visits: usize,
    young: usize,
    forwarded: usize,
    promoted: usize,
    old: usize,
    old_marked: usize,
    kinds: Vec<[usize; 2]>,
}

/// Only available while all mutator views are suspended by `collect`.
pub struct Tracer<'a> {
    inner: TracerKind<'a>,
}

enum TracerKind<'a> {
    Serial(SerialTracer<'a>),
    #[cfg(not(target_family = "wasm"))]
    Parallel(parallel::ParallelTracer<'a>),
    #[cfg(not(target_family = "wasm"))]
    Repair(parallel::RepairTracer<'a>),
}

struct SerialTracer<'a> {
    #[cfg(feature = "trace-counts")]
    counts: &'a mut TraceCounts,
    forwarded: &'a mut Forwarding,
    variables: &'a [VariableObject],
    ranges: &'a [ChunkRange],
    range_index: &'a RangeIndex,
    spaces: &'a [Space],
    old: &'a mut OldHeap,
    descriptors: &'static [Descriptor],
    major: bool,
    compaction: Option<&'a Compaction>,
    pending: &'a mut Worklist,
}

impl<'a> Tracer<'a> {
    #[inline]
    fn serial(&mut self) -> &mut SerialTracer<'a> {
        match &mut self.inner {
            TracerKind::Serial(serial) => serial,
            #[cfg(not(target_family = "wasm"))]
            _ => unreachable!("serial collection driver"),
        }
    }

    /// Relocate an exact or interior managed pointer, leaving independent
    /// external pointers unchanged. Each compaction edge is visited once.
    ///
    /// # Safety
    /// The pointer must satisfy `RawHeap::collect`'s lifetime/rooting contract.
    #[inline(always)]
    pub unsafe fn relocate(&mut self, pointer: Pointer) -> Pointer {
        match &mut self.inner {
            TracerKind::Serial(serial) => serial.relocate(pointer),
            #[cfg(not(target_family = "wasm"))]
            TracerKind::Parallel(parallel) => parallel.relocate(pointer),
            #[cfg(not(target_family = "wasm"))]
            TracerKind::Repair(repair) => repair.relocate(pointer),
        }
    }

    /// Relocate the exact start of an object with a statically known layout.
    ///
    /// # Safety
    /// In addition to `relocate`, the descriptor at KIND must have size and
    /// alignment matching T; interiors must use `relocate` instead.
    #[inline(always)]
    pub unsafe fn relocate_fixed<T, const KIND: usize>(
        &mut self,
        pointer: NonNull<T>,
    ) -> NonNull<T> {
        match &mut self.inner {
            // SAFETY: forwarded fixed-layout contract.
            TracerKind::Serial(serial) => unsafe { serial.relocate_fixed::<T, KIND>(pointer) },
            #[cfg(not(target_family = "wasm"))]
            TracerKind::Parallel(parallel) => parallel.relocate_fixed::<T, KIND>(pointer),
            #[cfg(not(target_family = "wasm"))]
            TracerKind::Repair(repair) => repair.relocate(pointer.cast()).cast(),
        }
    }

    /// Rewrite an explicit pointer slot.
    ///
    /// # Safety
    /// The slot must meet `relocate`'s pointer requirements.
    pub unsafe fn visit(&mut self, slot: &mut Pointer) {
        // SAFETY: forwarded caller contract.
        *slot = unsafe { self.relocate(*slot) };
    }
}

impl SerialTracer<'_> {
    /// Relocate one explicit pointer, preserving its offset within its owner.
    /// External/permanent addresses are unchanged. Already rewritten to-space
    /// addresses are unchanged during promotion. Compaction rewrites each edge
    /// exactly once; do not revisit or dereference a rewritten edge.
    ///
    /// The pointer contract is that of the public [`Tracer::relocate`].
    #[inline(always)]
    pub fn relocate(&mut self, pointer: Pointer) -> Pointer {
        #[cfg(feature = "trace-counts")]
        {
            self.counts.visits += 1;
        }
        if self.major {
            return self.relocate_major(pointer);
        }
        self.relocate_young(pointer)
    }

    /// Relocate the start of a fixed-size object with a statically known layout.
    /// This avoids generic interior-pointer and variable-layout lookup work.
    ///
    /// # Safety
    /// In addition to `relocate`'s contract, every managed pointer must denote
    /// the exact payload start of kind `KIND`. That descriptor must have
    /// `fixed_size: Some(size_of::<T>())` and `alignment: align_of::<T>()`.
    /// External pointers remain unchanged. Compaction's once-per-slot rule
    /// applies here too; a rewritten destination must not be visited again.
    #[inline(always)]
    pub unsafe fn relocate_fixed<T, const KIND: usize>(
        &mut self,
        pointer: NonNull<T>,
    ) -> NonNull<T> {
        debug_assert_eq!(self.descriptors[KIND].fixed_size, Some(size_of::<T>()));
        debug_assert_eq!(self.descriptors[KIND].alignment, align_of::<T>());
        #[cfg(feature = "trace-counts")]
        {
            self.counts.visits += 1;
        }
        if self.major {
            return self.relocate_fixed_major::<T, KIND>(pointer);
        }
        self.relocate_fixed_young::<T, KIND>(pointer)
    }

    #[inline(never)]
    fn relocate_fixed_major<T, const KIND: usize>(&mut self, pointer: NonNull<T>) -> NonNull<T> {
        if let Some(plan) = self.compaction {
            return self
                .old
                .compacted(pointer.cast(), self.descriptors, plan)
                .expect("strong edge was marked")
                .cast();
        }
        // SAFETY: relocate_fixed establishes the exact fixed-layout contract.
        if let Some((work, fresh)) = unsafe { self.old.mark_fixed::<T, KIND>(pointer) } {
            #[cfg(feature = "trace-counts")]
            {
                self.counts.old += 1;
                self.counts.old_marked += usize::from(fresh);
            }
            if fresh {
                self.pending.push(work);
            }
            return pointer;
        }
        self.relocate_fixed_young::<T, KIND>(pointer)
    }

    #[inline(always)]
    fn relocate_fixed_young<T, const KIND: usize>(&mut self, pointer: NonNull<T>) -> NonNull<T> {
        let address = pointer.as_ptr() as usize;
        let Some(range) = self.range_index.find(self.ranges, address) else {
            return pointer;
        };
        // The caller identifies the layout and guarantees an exact object
        // start. No variable-object rank or interior-offset repair is needed.
        let extent = size_of::<T>().max(1).next_multiple_of(align_of::<T>());
        let relative = address - range.pointer.as_ptr() as usize;
        debug_assert_eq!(range.kind, KIND);
        debug_assert_eq!(relative % extent, 0);
        let local = relative / extent;
        self.forward_with(
            &Located {
                index: range.first + local,
                small: if extent < size_of::<Pointer>() {
                    range.small_first + local
                } else {
                    usize::MAX
                },
                offset: 0,
                source: pointer.cast(),
                bytes: size_of::<T>(),
                kind: KIND,
            },
            Self::promote_fixed::<T, KIND>,
        )
        .cast()
    }

    // Separate the major-only branches so array/descriptor tracing can inline
    // the ordinary nursery fast path without duplicating the whole collector.
    #[inline(never)]
    fn relocate_major(&mut self, pointer: Pointer) -> Pointer {
        if let Some(plan) = self.compaction {
            return self
                .old
                .compacted(pointer, self.descriptors, plan)
                .expect("strong edge was marked");
        }
        if let Some((work, fresh)) = self.old.mark(pointer, self.descriptors) {
            #[cfg(feature = "trace-counts")]
            {
                self.counts.old += 1;
                self.counts.old_marked += usize::from(fresh);
            }
            if fresh {
                self.pending.push(work);
            }
            return pointer;
        }
        self.relocate_young(pointer)
    }

    #[inline(always)]
    fn relocate_young(&mut self, pointer: Pointer) -> Pointer {
        let Some(object) = find(self.variables, self.ranges, self.range_index, pointer) else {
            return pointer;
        };
        let target = self.forward_with(&object, Self::promote);
        // SAFETY: offset is inside the original payload and the same-sized copy.
        unsafe { Pointer::new_unchecked(target.as_ptr().add(object.offset)) }
    }

    #[inline(always)]
    fn forward_with(
        &mut self,
        object: &Located,
        promote: impl FnOnce(&mut Self, &Located) -> Pointer,
    ) -> Pointer {
        #[cfg(feature = "trace-counts")]
        {
            self.counts.young += 1;
        }
        match self.forwarded.get(object) {
            Some(target) => {
                #[cfg(feature = "trace-counts")]
                {
                    self.counts.forwarded += 1;
                    self.counts.kinds[object.kind][1] += 1;
                }
                target
            }
            None => {
                #[cfg(feature = "trace-counts")]
                {
                    self.counts.promoted += 1;
                    self.counts.kinds[object.kind][0] += 1;
                }
                promote(self, object)
            }
        }
    }

    // Keep allocation, copying and queue growth off the per-edge fast path.
    #[inline(never)]
    fn promote(&mut self, object: &Located) -> Pointer {
        let (target, work) = self.old.allocate(
            object.kind,
            &self.spaces[object.kind],
            object.bytes,
            self.major,
        );
        // SAFETY: pre-reserved destination fits this initialized payload.
        // The nursery copy no longer owns resources after forwarding.
        unsafe {
            let source = object.source.as_ptr();
            let destination = target.as_ptr();
            // Small evaluator records dominate promotion. Constant lengths
            // let the compiler use inline loads/stores instead of calling
            // memcpy once per object; arbitrary-sized payloads still work.
            match object.bytes {
                8 => std::ptr::copy_nonoverlapping(source, destination, 8),
                16 => std::ptr::copy_nonoverlapping(source, destination, 16),
                24 => std::ptr::copy_nonoverlapping(source, destination, 24),
                32 => std::ptr::copy_nonoverlapping(source, destination, 32),
                48 => std::ptr::copy_nonoverlapping(source, destination, 48),
                bytes => std::ptr::copy_nonoverlapping(source, destination, bytes),
            }
        }
        self.forwarded.set(object, target);
        self.pending.push(work);
        target
    }

    #[inline(never)]
    fn promote_fixed<T, const KIND: usize>(&mut self, object: &Located) -> Pointer {
        let (target, work) = self.old.allocate_fixed::<T, KIND>(self.major);
        // SAFETY: the caller guarantees this fixed layout and an exact payload
        // start; preflight reserved the matching destination class.
        unsafe {
            std::ptr::copy_nonoverlapping(object.source.as_ptr(), target.as_ptr(), size_of::<T>());
        }
        self.forwarded.set(object, target);
        self.pending.push(work);
        target
    }
}

/// The completed forwarding map, borrowed only for weak-reference repair.
pub struct Relocation<'a> {
    old: &'a OldHeap,
    descriptors: &'static [Descriptor],
    major: bool,
    compaction: Option<&'a Compaction>,
    forwarded: &'a Forwarding,
    variables: &'a [VariableObject],
    ranges: &'a [ChunkRange],
    range_index: &'a RangeIndex,
}

impl Relocation<'_> {
    /// Whether old-generation objects can die or move in this collection. In
    /// a minor collection they do neither: only nursery addresses change, and
    /// every nursery survivor is promoted.
    pub fn collects_old(&self) -> bool {
        self.major
    }

    /// `None` for a dead managed owner, the relocated address for a live one,
    /// or the unchanged address for an external/permanent pointer. Interior
    /// offsets are preserved. This lookup never makes an object live.
    pub fn lookup(&self, pointer: Pointer) -> Option<Pointer> {
        let Some(object) = find(self.variables, self.ranges, self.range_index, pointer) else {
            return if let Some(plan) = self.compaction {
                self.old.compacted(pointer, self.descriptors, plan)
            } else {
                self.old.weak(pointer, self.descriptors, self.major)
            };
        };
        self.forwarded.get(&object).and_then(|target| {
            let target = if let Some(plan) = self.compaction {
                self.old.compacted(target, self.descriptors, plan)?
            } else {
                target
            };
            // SAFETY: offset is inside this object's same-sized destination.
            Some(unsafe { Pointer::new_unchecked(target.as_ptr().add(object.offset)) })
        })
    }
}

fn reserved<T>(capacity: usize) -> Result<Vec<T>> {
    #[cfg(test)]
    reservation_checkpoint()?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|_| Error::OutOfMemory)?;
    Ok(result)
}

// Deterministic reservation failures exercise collection's transaction boundary
// without relying on the system allocator or exhausting the machine's memory.
#[cfg(test)]
std::thread_local! {
    static FAIL_RESERVATION_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn reservation_checkpoint() -> Result<()> {
    FAIL_RESERVATION_AFTER.with(|count| match count.get() {
        Some(0) => Err(Error::OutOfMemory),
        Some(left) => {
            count.set(Some(left - 1));
            Ok(())
        }
        None => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Owned(Rc<Cell<usize>>);
    impl Drop for Owned {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
    unsafe fn drop_owned(pointer: Pointer, _: usize) {
        // SAFETY: descriptor only applies to initialized Owned payloads.
        unsafe { pointer.cast::<Owned>().as_ptr().drop_in_place() };
    }

    static DESCRIPTORS: [Descriptor; 2] = [
        Descriptor {
            name: "fixed owned",
            fixed_size: Some(size_of::<Owned>()),
            alignment: align_of::<Owned>(),
            trace,
            drop: Some(drop_owned),
        },
        Descriptor {
            name: "variable owned",
            fixed_size: None,
            alignment: align_of::<Owned>(),
            trace,
            drop: Some(drop_owned),
        },
    ];

    fn chunk(layout: Layout) -> Chunk {
        Chunk {
            // SAFETY: the layouts used below are nonempty.
            pointer: Pointer::new(unsafe { alloc(layout) }).unwrap(),
            layout,
            used: 0,
            first_variable: 0,
        }
    }

    #[test]
    fn spare_chunks_keep_only_the_pooled_layout_up_to_the_limit() {
        let pooled = Layout::from_size_align(1 << 20, 4096).unwrap();
        let mut spare = SpareChunks::new(1 << 20);
        assert_eq!(spare.take(pooled), None);

        let first = chunk(pooled);
        let address = first.pointer;
        spare.keep(first, 2 << 20);
        spare.keep(chunk(pooled), 2 << 20);
        assert_eq!(spare.bytes(), 2 << 20);
        // Over the limit, larger than a nursery chunk, or otherwise
        // aligned: freed, not kept.
        spare.keep(chunk(pooled), 2 << 20);
        spare.keep(
            chunk(Layout::from_size_align(2 << 20, 4096).unwrap()),
            usize::MAX,
        );
        spare.keep(
            chunk(Layout::from_size_align(1 << 20, 8).unwrap()),
            usize::MAX,
        );
        assert_eq!(spare.bytes(), 2 << 20);

        assert_eq!(
            spare.take(Layout::from_size_align(1 << 20, 8).unwrap()),
            None
        );
        let second = spare.take(pooled).unwrap();
        assert_eq!(spare.take(pooled), Some(address));
        assert_eq!(spare.take(pooled), None);
        // SAFETY: both were allocated with `pooled` and are owned here again.
        unsafe {
            dealloc(second.as_ptr(), pooled);
            dealloc(address.as_ptr(), pooled);
        }
    }

    #[test]
    fn a_collection_hands_its_nursery_chunks_to_the_next_cycle() {
        unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
        static KINDS: [Descriptor; 1] = [Descriptor {
            name: "word",
            fixed_size: Some(size_of::<usize>()),
            alignment: align_of::<usize>(),
            trace,
            drop: None,
        }];
        let mut heap = RawHeap::new(
            Config {
                chunk_bytes: 1 << 20,
                threshold_bytes: 4 << 20,
                steady_threshold_bytes: Some(1 << 20),
                ..Config::default()
            },
            &KINDS,
        )
        .unwrap();
        let words = (1 << 20) / size_of::<usize>();
        // SAFETY: nothing allocated here is read or kept across a collection.
        unsafe {
            // Three chunks in the first nursery.
            for _ in 0..2 * words + 1 {
                heap.allocate_slow(0, size_of::<usize>()).unwrap();
            }
            assert_eq!(heap.nursery_capacity_bytes(), 3 << 20);
            heap.collect_minor(|_| {}, |_| {}).unwrap();
            // The steady nursery is one chunk: one is kept, two are freed.
            assert_eq!(heap.spare.lock().bytes(), 1 << 20);
            heap.allocate_slow(0, size_of::<usize>()).unwrap();
            assert_eq!(
                heap.spare.lock().bytes(),
                0,
                "the next cycle starts in the kept chunk"
            );
            assert_eq!(heap.nursery_capacity_bytes(), 1 << 20);
        }
    }

    #[test]
    fn a_donor_refills_from_the_chunks_the_collector_emptied() {
        unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
        static KINDS: [Descriptor; 1] = [Descriptor {
            name: "word",
            fixed_size: Some(size_of::<usize>()),
            alignment: align_of::<usize>(),
            trace,
            drop: None,
        }];
        let config = Config {
            chunk_bytes: 1 << 20,
            threshold_bytes: 2 << 20,
            workers: 1,
            ..Config::default()
        };
        let spare = SpareChunkPool::new(&config);
        let mut collector = RawHeap::with_spare_chunks(config, &KINDS, spare.clone()).unwrap();
        let mut donor = RawHeap::with_spare_chunks(config, &KINDS, spare.clone()).unwrap();
        // SAFETY: nothing allocated here is read or kept across a collection.
        unsafe {
            let first = donor.allocate_slow(0, size_of::<usize>()).unwrap();
            collector.absorb_nursery(&mut donor).unwrap();
            collector.collect_minor(|_| {}, |_| {}).unwrap();
            assert_eq!(spare.lock().bytes(), 1 << 20);
            let second = donor.allocate_slow(0, size_of::<usize>()).unwrap();
            assert_eq!(second, first, "the donor's next chunk is the emptied one");
            assert_eq!(spare.lock().bytes(), 0);
        }
    }

    #[test]
    fn absorbed_nurseries_preserve_cross_worker_edges_interiors_and_barriers() {
        unsafe fn edge(pointer: Pointer, _: usize, tracer: &mut Tracer<'_>) {
            let slot = unsafe { &mut *pointer.cast::<Pointer>().as_ptr() };
            *slot = unsafe { tracer.relocate(*slot) };
        }
        static KINDS: [Descriptor; 2] = [
            Descriptor {
                name: "edge",
                fixed_size: Some(size_of::<Pointer>()),
                alignment: align_of::<Pointer>(),
                trace: edge,
                drop: None,
            },
            Descriptor {
                name: "bytes",
                fixed_size: None,
                alignment: 1,
                trace,
                drop: None,
            },
        ];
        let config = Config {
            chunk_bytes: 64,
            threshold_bytes: usize::MAX,
            workers: 1,
            ..Config::default()
        };
        let mut collector = RawHeap::new(config, &KINDS).unwrap();
        let mut a = RawHeap::new(config, &KINDS).unwrap();
        let mut b = RawHeap::new(config, &KINDS).unwrap();
        unsafe {
            // Several variable chunks in each nursery exercise variable-rank
            // offsets when the nurseries are concatenated.
            for heap in [&mut collector, &mut a, &mut b] {
                for size in [17, 80, 33] {
                    let bytes = heap.allocate_slow(1, size).unwrap();
                    bytes.as_ptr().write_bytes(0, size);
                }
            }
            let bytes = b.allocate_slow(1, 39).unwrap();
            bytes.as_ptr().write_bytes(42, 39);
            let interior = Pointer::new_unchecked(bytes.as_ptr().add(7));
            let mut root = a.allocate_slow(0, size_of::<Pointer>()).unwrap();
            root.cast::<Pointer>().as_ptr().write(interior);
            let capacity = collector.nursery_capacity_bytes()
                + a.nursery_capacity_bytes()
                + b.nursery_capacity_bytes();
            collector.absorb_nursery(&mut a).unwrap();
            collector.absorb_nursery(&mut b).unwrap();
            assert_eq!(collector.nursery_capacity_bytes(), capacity);
            assert_eq!(a.nursery_capacity_bytes() + b.nursery_capacity_bytes(), 0);
            collector
                .collect_minor(|tracer| root = tracer.relocate(root), |_| {})
                .unwrap();
            assert_eq!(*(*root.cast::<Pointer>().as_ptr()).as_ptr(), 42);
            // A donor allocates again, and the old parent receives a young
            // edge. Replaying its barrier on the common collector retains it.
            let bytes = a.allocate_slow(1, 93).unwrap();
            bytes.as_ptr().write_bytes(73, 93);
            root.cast::<Pointer>()
                .as_ptr()
                .write(Pointer::new_unchecked(bytes.as_ptr().add(9)));
            collector.absorb_nursery(&mut a).unwrap();
            collector.write_barrier(root);
            collector
                .collect_minor(|tracer| root = tracer.relocate(root), |_| {})
                .unwrap();
            assert_eq!(*(*root.cast::<Pointer>().as_ptr()).as_ptr(), 73);
            collector
                .collect(|tracer| root = tracer.relocate(root), |_| {})
                .unwrap();
            assert_eq!(*(*root.cast::<Pointer>().as_ptr()).as_ptr(), 73);
            collector.collect(|_| {}, |_| {}).unwrap();
            assert_eq!(collector.used_bytes(), 0);
        }
    }

    #[test]
    fn large_chunk_alignment_preserves_descriptor_strides_and_stronger_alignment() {
        for alignment in [8, 8192] {
            let descriptor = Descriptor {
                name: "aligned fixed",
                fixed_size: Some(8),
                alignment,
                trace,
                drop: None,
            };
            let mut space = Space::new(&descriptor).unwrap();
            let extent = space.extent(8).unwrap();
            space
                .add_chunk(1 << 20, &mut SpareChunks::new(1 << 20))
                .unwrap();
            assert_eq!(space.chunks[0].layout.align(), alignment.max(4096));
            assert_eq!(space.chunks[0].layout.size(), 1 << 20);
            assert_eq!(
                space.chunks[0].pointer.as_ptr().addr() % alignment.max(4096),
                0
            );
            let first = space.try_allocate(8, extent).unwrap();
            let second = space.try_allocate(8, extent).unwrap();
            assert_eq!(second.as_ptr().addr() - first.as_ptr().addr(), alignment);
            assert_eq!(space.used(), 2 * alignment);
            assert_eq!(space.capacity, 1 << 20);
        }
        static FIXED: [Descriptor; 1] = [Descriptor {
            name: "word",
            fixed_size: Some(size_of::<usize>()),
            alignment: align_of::<usize>(),
            trace,
            drop: None,
        }];
        let mut heap = RawHeap::new(
            Config {
                workers: 1,
                chunk_bytes: 1 << 20,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: usize::MAX,
                stress: false,
            },
            &FIXED,
        )
        .unwrap();
        let mut root = unsafe { heap.allocate_slow(0, size_of::<usize>()).unwrap() };
        unsafe { root.cast::<usize>().as_ptr().write(42) };
        assert_eq!(root.as_ptr().addr() % 4096, 0);
        unsafe {
            heap.collect(|tracer| root = tracer.relocate(root), |_| {})
                .unwrap();
        }
        assert_eq!(unsafe { *root.cast::<usize>().as_ptr() }, 42);
        assert_eq!(heap.used_bytes(), size_of::<usize>());
    }

    #[test]
    fn bounded_slot_division_preserves_all_small_offsets_and_large_boundaries() {
        for stride in [1, 2, 3, 7, 8, 12, 16, 24, 31, 32, 48, 64, 511, 1024, 4096] {
            let bytes = 65536;
            let reciprocal = slot_reciprocal(stride, bytes);
            assert_ne!(reciprocal, 0);
            for offset in 0..bytes {
                assert_eq!(slot_index(offset, stride, reciprocal), offset / stride);
            }
            for bytes in [1, 1 << 21, 1 << 30, usize::MAX] {
                let reciprocal = slot_reciprocal(stride, bytes);
                for offset in [0, bytes / 2, bytes - 1] {
                    assert_eq!(slot_index(offset, stride, reciprocal), offset / stride);
                }
                let boundary = (bytes - 1) / stride * stride;
                for offset in [boundary.saturating_sub(1), boundary] {
                    assert_eq!(slot_index(offset, stride, reciprocal), offset / stride);
                }
            }
        }
        assert_eq!(slot_reciprocal(usize::MAX, 2), 0);
        assert_eq!(slot_index(usize::MAX - 1, usize::MAX, 0), 0);
    }

    #[test]
    fn bitmap_rank_counts_set_bits() {
        let index = RangeIndex::new(&mut [], &[]).unwrap();
        let mut bits = 0x123456789abcdef0u64;
        for _ in 0..10000 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            assert_eq!(index.count_starts(bits), bits.count_ones() as usize);
        }
        assert_eq!(index.count_starts(0), 0);
        assert_eq!(index.count_starts(u64::MAX), 64);
        for bit in 0..64 {
            assert_eq!(index.count_starts(u64::MAX >> bit), 64 - bit);
        }
    }

    #[test]
    fn chunk_lookup_handles_unordered_chunks_padding_and_interior_offsets() {
        let mut backing = [0u8; 128];
        let base = backing.as_mut_ptr();
        let pointer = |offset| Pointer::new(base.wrapping_add(offset)).unwrap();
        // Object ordinals follow allocation order, not virtual-address order.
        let variables = [
            VariableObject {
                offset: 8,
                bytes: 3,
            },
            VariableObject {
                offset: 24,
                bytes: 9,
            },
        ];
        let mut ranges = [
            ChunkRange {
                reciprocal: 0,
                pointer: pointer(0),
                end: pointer(40).as_ptr() as usize,
                first: 2,
                count: 2,
                kind: 1,
                fixed_size: None,
                fixed_extent: None,
                variable_first: 0,
                variable_block_first: 0,
                small_first: usize::MAX,
            },
            ChunkRange {
                reciprocal: 0,
                pointer: pointer(64),
                end: pointer(96).as_ptr() as usize,
                first: 0,
                count: 2,
                kind: 0,
                fixed_size: Some(12),
                fixed_extent: Some(16),
                variable_first: 0,
                variable_block_first: 0,
                small_first: usize::MAX,
            },
        ];
        let index = RangeIndex::new(&mut ranges, &variables).unwrap();
        for (address, expected) in [
            (0, None),
            (8, Some((2, 0))),
            (10, Some((2, 2))),
            (11, None),
            (24, Some((3, 0))),
            (32, Some((3, 8))),
            (33, None),
            (40, None),
            (64, Some((0, 0))),
            (75, Some((0, 11))),
            (76, None),
            (80, Some((1, 0))),
            (91, Some((1, 11))),
            (96, None),
        ] {
            assert_eq!(
                find(&variables, &ranges, &index, pointer(address))
                    .map(|object| (object.index, object.offset)),
                expected,
                "{address:x}"
            );
        }
    }

    #[test]
    fn segmented_worklist_preserves_order_across_blocks_and_reuse() {
        let capacity = Worklist::BLOCK * 2 + 17;
        let mut work = Worklist::new(capacity).unwrap();
        for round in 0..3 {
            for i in 0..capacity {
                work.push(round * capacity + i);
            }
            for i in (0..capacity).rev() {
                assert_eq!(work.pop(), Some(round * capacity + i));
            }
            assert_eq!(work.pop(), None);
            assert_eq!(work.pop(), None);
        }
    }

    #[test]
    fn variable_rank_index_matches_owners_across_bitmap_and_chunk_boundaries() {
        let mut backing = vec![0usize; 2048];
        let base = backing.as_mut_ptr().cast::<u8>();
        let variables = [
            (8, 100),
            (504, 0),
            (512, 128),
            (1024, 1000),
            (2040, 2048),
            (4096, 4096),
            (8200, 32),
        ]
        .map(|(offset, bytes)| VariableObject { offset, bytes });
        let mut ranges = [ChunkRange {
            reciprocal: 0,
            pointer: Pointer::new(base).unwrap(),
            end: base as usize + 8240,
            first: 17,
            count: variables.len(),
            kind: 1,
            fixed_size: None,
            fixed_extent: None,
            variable_first: 0,
            variable_block_first: 0,
            small_first: usize::MAX,
        }];
        let index = RangeIndex::new(&mut ranges, &variables).unwrap();
        for address in 0..8248 {
            let expected = variables.iter().enumerate().find_map(|(i, object)| {
                (address >= object.offset && address < object.offset + object.bytes.max(1))
                    .then_some((17 + i, address.wrapping_sub(object.offset)))
            });
            let pointer = Pointer::new(base.wrapping_add(address)).unwrap();
            assert_eq!(
                find(&variables, &ranges, &index, pointer).map(|o| (o.index, o.offset)),
                expected,
                "address {address}"
            );
        }
    }

    #[test]
    fn variable_reservation_refill_failure_and_cancellation_preserve_heap() {
        struct Reset(Option<usize>);
        impl Drop for Reset {
            fn drop(&mut self) {
                FAIL_RESERVATION_AFTER.set(self.0);
            }
        }
        for failure in 0..=2 {
            let drops = Rc::new(Cell::new(0));
            let mut heap = RawHeap::new(
                Config {
                    chunk_bytes: 32,
                    ..Config::default()
                },
                &DESCRIPTORS,
            )
            .unwrap();
            let mut root = unsafe { heap.allocate_slow(1, size_of::<Owned>()).unwrap() };
            unsafe {
                root.cast::<Owned>().as_ptr().write(Owned(drops.clone()));
            }
            let used = heap.used_bytes();
            let capacity = heap.capacity_bytes();
            let maximum = 128;
            let extent = heap.allocation_extent(1, maximum).unwrap();
            {
                let _reset = Reset(FAIL_RESERVATION_AFTER.replace(Some(failure)));
                let reservation = heap.reserve_variable(1, maximum);
                if failure < 2 {
                    assert!(matches!(reservation, Err(Error::OutOfMemory)));
                } else {
                    let reservation = reservation.unwrap();
                    assert_eq!(reservation.space.used(), used);
                    // Even partial writes followed by cancellation never turn
                    // into an Owned payload for trace/drop to interpret.
                    unsafe {
                        reservation.payload().as_ptr().write(0xff);
                    }
                    drop(reservation);
                }
            }
            assert_eq!(heap.used_bytes(), used);
            assert_eq!(
                heap.capacity_bytes(),
                capacity + if failure == 2 { extent } else { 0 }
            );
            assert_eq!(
                heap.capacity_bytes(),
                heap.spaces.iter().map(|space| space.capacity).sum()
            );
            unsafe {
                heap.collect(|tracer| tracer.visit(&mut root), |_| {})
                    .unwrap();
            }
            assert_eq!(drops.get(), 0);
            assert!(unsafe { Rc::ptr_eq(&root.cast::<Owned>().as_ref().0, &drops) });
            drop(heap);
            assert_eq!(drops.get(), 1);
        }
    }

    #[test]
    fn paired_fixed_reservation_failure_never_exposes_uninitialized_objects() {
        static PAIR: [Descriptor; 2] = [
            Descriptor {
                name: "first owned",
                fixed_size: Some(size_of::<Owned>()),
                alignment: align_of::<Owned>(),
                trace,
                drop: Some(drop_owned),
            },
            Descriptor {
                name: "second owned",
                fixed_size: Some(size_of::<Owned>()),
                alignment: align_of::<Owned>(),
                trace,
                drop: Some(drop_owned),
            },
        ];
        struct Reset(Option<usize>);
        impl Drop for Reset {
            fn drop(&mut self) {
                FAIL_RESERVATION_AFTER.set(self.0);
            }
        }
        // Each space refill has two fallible reservation boundaries. In cases
        // 2/3 the first space has an empty new chunk when the second fails.
        for failure in 0..=4 {
            let drops = Rc::new(Cell::new(0));
            let mut heap = RawHeap::new(
                Config {
                    chunk_bytes: 2 * size_of::<Owned>(),
                    ..Config::default()
                },
                &PAIR,
            )
            .unwrap();
            let mut roots = [Pointer::dangling(); 2];
            for (kind, root) in roots.iter_mut().enumerate() {
                unsafe {
                    *root = heap.allocate_slow(kind, size_of::<Owned>()).unwrap();
                    root.cast::<Owned>().as_ptr().write(Owned(drops.clone()));
                }
            }
            let used = heap.used_bytes();
            let capacity = heap.capacity_bytes();
            let result = {
                let _reset = Reset(FAIL_RESERVATION_AFTER.replace(Some(failure)));
                unsafe {
                    heap.allocate_known_fixed_pair::<{ size_of::<Owned>() }, { size_of::<Owned>() }>(0, 3, 1, 3)
                }
            };
            if failure == 4 {
                let (a, b) = result.unwrap();
                // Joint initialization finishes before any further heap call.
                for pointer in [a, b] {
                    for index in 0..3 {
                        unsafe {
                            pointer
                                .cast::<Owned>()
                                .as_ptr()
                                .add(index)
                                .write(Owned(drops.clone()))
                        };
                    }
                }
                assert_eq!(heap.used_bytes(), used + 6 * size_of::<Owned>());
            } else {
                assert_eq!(result, Err(Error::OutOfMemory), "reservation {failure}");
                assert_eq!(heap.used_bytes(), used);
                assert_eq!(
                    heap.capacity_bytes(),
                    capacity
                        + if failure >= 2 {
                            3 * size_of::<Owned>()
                        } else {
                            0
                        }
                );
            }
            assert_eq!(
                heap.capacity_bytes(),
                heap.spaces.iter().map(|space| space.capacity).sum()
            );
            assert_eq!(drops.get(), 0);
            unsafe {
                heap.collect(
                    |tracer| {
                        for root in &mut roots {
                            tracer.visit(root);
                        }
                    },
                    |_| {},
                )
                .unwrap();
                for root in roots {
                    assert!(Rc::ptr_eq(&root.cast::<Owned>().as_ref().0, &drops));
                }
            }
            assert_eq!(heap.used_bytes(), used);
            assert_eq!(drops.get(), if failure == 4 { 6 } else { 0 });
            drop(heap);
            assert_eq!(drops.get(), if failure == 4 { 8 } else { 2 });
        }
    }

    #[test]
    fn every_reservation_failure_leaves_roots_and_ownership_intact() {
        // Walk every reservation boundary, including compaction scratch,
        // stopping only when the complete collection succeeds.
        let mut reached_success = false;
        for fail_after in 0..100 {
            let drops = Rc::new(Cell::new(0));
            let mut heap = RawHeap::new(
                Config {
                    steady_threshold_bytes: Some(64),
                    ..Config::default()
                },
                &DESCRIPTORS,
            )
            .unwrap();
            let mut roots = [Pointer::dangling(); 2];
            // SAFETY: initialize each payload immediately, before the next
            // allocation; all owned resources move into those payloads.
            unsafe {
                for (kind, root) in roots.iter_mut().enumerate() {
                    *root = heap.allocate_slow(kind, size_of::<Owned>()).unwrap();
                    root.cast::<Owned>().as_ptr().write(Owned(drops.clone()));
                }
            }
            let before_roots = roots;
            let before_used = heap.used_bytes();
            let before_capacity = heap.capacity_bytes();
            let before_budget = heap.major_threshold;
            let before_nursery = heap.threshold;
            let called = Cell::new(false);
            FAIL_RESERVATION_AFTER.with(|count| count.set(Some(fail_after)));
            // SAFETY: trace both roots; callbacks cannot run on reservation
            // failure. The failure injection only changes fallible allocations.
            let result = unsafe {
                heap.collect(
                    |tracer| {
                        called.set(true);
                        for root in &mut roots {
                            tracer.visit(root);
                        }
                    },
                    |_| {
                        called.set(true);
                    },
                )
            };
            FAIL_RESERVATION_AFTER.with(|count| count.set(None));
            if result.is_ok() {
                assert!(called.get());
                assert_eq!(drops.get(), 0);
                drop(heap);
                assert_eq!(drops.get(), 2);
                reached_success = true;
                break;
            }
            assert_eq!(result, Err(Error::OutOfMemory), "reservation {fail_after}");
            assert!(!called.get());
            assert_eq!(roots, before_roots);
            assert_eq!(heap.used_bytes(), before_used);
            assert!(heap.capacity_bytes() >= before_capacity);
            assert_eq!(heap.major_threshold, before_budget);
            assert_eq!(heap.threshold, before_nursery);
            assert_eq!(heap.stats().collections, 0);
            assert_eq!(drops.get(), 0);
            // SAFETY: no pointers changed; roots still describe the graph.
            unsafe {
                heap.collect(
                    |tracer| {
                        for root in &mut roots {
                            tracer.visit(root);
                        }
                    },
                    |_| {},
                )
                .unwrap();
            }
            assert_eq!(drops.get(), 0);
            assert_eq!(heap.threshold, 64);
            drop(heap);
            assert_eq!(drops.get(), 2);
        }
        assert!(reached_success);
    }
    #[test]
    fn major_budget_backs_off_for_retention_and_resets_after_reclamation() {
        for steady_threshold_bytes in [None, Some(32)] {
            let drops = Rc::new(Cell::new(0));
            let mut heap = RawHeap::new(
                Config {
                    steady_threshold_bytes,
                    trace_collections: false,
                    threshold_bytes: 64,
                    ..Config::default()
                },
                &DESCRIPTORS,
            )
            .unwrap();
            let mut roots = Vec::new();
            for _ in 0..128 {
                unsafe {
                    let p = heap.allocate_slow(0, size_of::<Owned>()).unwrap();
                    p.cast::<Owned>().as_ptr().write(Owned(drops.clone()));
                    roots.push(p);
                }
            }
            for growth in [4, 8, 8] {
                unsafe {
                    heap.collect(
                        |t| {
                            for p in &mut roots {
                                t.visit(p);
                            }
                        },
                        |_| {},
                    )
                    .unwrap();
                }
                assert_eq!(heap.major_growth, growth);
                assert_eq!(
                    heap.major_threshold,
                    if steady_threshold_bytes.is_some() {
                        heap.old_bytes() + 64
                    } else {
                        heap.old_bytes() * growth
                    }
                );
                assert_eq!(drops.get(), 0);
            }
            roots.truncate(1);
            unsafe {
                heap.collect(|t| t.visit(&mut roots[0]), |_| {}).unwrap();
            }
            assert_eq!(heap.major_growth, 2);
            assert_eq!(heap.major_threshold, 64);
            assert_eq!(heap.stats().old_reclaimed_bytes, 127 * size_of::<Owned>());
            assert_eq!(drops.get(), 127);
        }
    }
}
