//! Frozen page metadata with atomic marking and exclusive promotion leases.
//! No reference to a mutable Page/OldHeap is manufactured on a worker.
use super::*;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;

pub(in super::super) const LAB: usize = 64;

struct Reservations<'a> {
    heap: &'a OldHeap,
    demands: &'a [usize],
    spaces: &'a [Space],
    chunk_bytes: usize,
    next: AtomicUsize,
    results: Vec<UnsafeCell<Result<Vec<Page>>>>,
}
// SAFETY: existing heap metadata/spaces are read-only. Each bin constructs its
// own empty pages and has one result writer. Empty pages own allocation storage
// only, never non-Send payload resources. Publication waits for every worker.
unsafe impl Sync for Reservations<'_> {}
impl Reservations<'_> {
    fn work(&self) {
        loop {
            let bin = self.next.fetch_add(1, Ordering::Relaxed);
            if bin >= self.demands.len() {
                return;
            }
            if self.demands[bin] <= self.heap.bins[bin].free {
                continue;
            }
            let result = self.heap.reserve_pages_range(
                self.demands,
                self.spaces,
                self.chunk_bytes,
                bin,
                bin + 1,
            );
            // SAFETY: this bin's result is assigned to exactly one worker.
            unsafe { self.results[bin].get().write(result) };
        }
    }
}
pub(in super::super) fn reserve_pages(
    heap: &OldHeap,
    demands: &[usize],
    spaces: &[Space],
    chunk_bytes: usize,
    pool: &mut super::super::pool::Pool,
) -> Result<Vec<Page>> {
    let mut results = reserved(demands.len())?;
    results.resize_with(demands.len(), || UnsafeCell::new(Ok(Vec::new())));
    let mut context = Reservations {
        heap,
        demands,
        spaces,
        chunk_bytes,
        next: AtomicUsize::new(0),
        results,
    };
    pool.run(|_| context.work());
    let mut count = 0usize;
    for result in &mut context.results {
        let pages = result.get_mut().as_ref().map_err(|error| *error)?;
        count = count
            .checked_add(pages.len())
            .ok_or(Error::AllocationTooLarge)?;
    }
    let mut pages = reserved(count)?;
    // Stable size-class/page order preserves compaction ordering and locality.
    for result in &mut context.results {
        pages.append(
            result
                .get_mut()
                .as_mut()
                .expect("all reservations succeeded"),
        );
    }
    Ok(pages)
}

#[derive(Clone, Copy)]
pub(in super::super) struct LocalAllocation {
    page: usize,
    next: usize,
    remaining: usize,
    linked: bool,
}
impl Default for LocalAllocation {
    fn default() -> Self {
        Self {
            page: END,
            next: 0,
            remaining: 0,
            linked: false,
        }
    }
}
impl LocalAllocation {
    #[inline]
    fn take(&mut self, page: &ParallelPage) -> usize {
        debug_assert_ne!(self.remaining, 0);
        let slot = self.next;
        self.remaining -= 1;
        self.next = if self.linked {
            // SAFETY: refill detached this chain under the bin lock. Only its
            // owning worker reads these links, before overwriting each payload.
            unsafe { page.pointer(slot).cast::<usize>().as_ptr().read_unaligned() }
        } else {
            slot + 1
        };
        slot
    }
}

struct Allocation {
    free: usize,
    bump: usize,
    free_count: usize,
    next_free: usize,
}
struct ParallelPage {
    pointer: Pointer,
    size: usize,
    kind: usize,
    stride: usize,
    reciprocal: u64,
    flags: *mut u8,
    lengths: *mut usize,
    variable: bool,
    slots: usize,
    next_dirty: usize,
    allocation: UnsafeCell<Allocation>,
}
impl ParallelPage {
    fn pointer(&self, slot: usize) -> Pointer {
        // SAFETY: slots come from this page's validated bounds or an exclusive lease.
        unsafe { Pointer::new_unchecked(self.pointer.as_ptr().add(slot * self.stride)) }
    }
    fn flag(&self, slot: usize) -> &AtomicU8 {
        // SAFETY: the pre-existing byte storage is exclusively accessed atomically
        // throughout this phase; AtomicU8 has byte size and alignment.
        unsafe { AtomicU8::from_ptr(self.flags.add(slot)) }
    }
    fn bytes(&self, slot: usize, descriptors: &[Descriptor]) -> usize {
        descriptors[self.kind].fixed_size.unwrap_or_else(|| {
            // SAFETY: published live slots have initialized lengths. Publication
            // of newly promoted pointers synchronizes after this length is set.
            unsafe { self.lengths.add(slot).read() }
        })
    }
}

pub(in super::super) struct ParallelOld<'a> {
    heap: &'a mut OldHeap,
    pages: Vec<ParallelPage>,
    lookup: Vec<usize>,
    bins: Vec<Mutex<usize>>,
    slot_bits: u32,
    dirty: usize,
}
// SAFETY: page metadata and lookup are frozen; flags are atomic. Allocation
// cursors are protected by their size-class mutex, while each leased payload
// and length slot has exactly one writer. heap itself is untouched until join.
unsafe impl Sync for ParallelOld<'_> {}

impl<'a> ParallelOld<'a> {
    pub fn new(heap: &'a mut OldHeap) -> Result<Self> {
        let mut pages = reserved(heap.pages.len())?;
        for page in &mut heap.pages {
            pages.push(ParallelPage {
                pointer: page.chunk.pointer,
                size: page.chunk.layout.size(),
                kind: page.kind,
                stride: page.stride,
                reciprocal: page.reciprocal,
                flags: page.flags.as_mut_ptr(),
                lengths: page.lengths.as_mut_ptr(),
                variable: !page.lengths.is_empty(),
                slots: page.flags.len(),
                next_dirty: page.next_dirty,
                allocation: UnsafeCell::new(Allocation {
                    free: page.free,
                    bump: page.bump,
                    free_count: page.free_count,
                    next_free: page.next_free,
                }),
            });
        }
        let mut bins = reserved(heap.bins.len())?;
        bins.extend(heap.bins.iter().map(|bin| Mutex::new(bin.head)));
        let mut lookup = reserved(heap.lookup.len())?;
        lookup.extend_from_slice(&heap.lookup);
        Ok(Self {
            slot_bits: heap.slot_bits,
            dirty: heap.dirty,
            heap,
            pages,
            lookup,
            bins,
        })
    }

    pub fn bin_count(&self) -> usize {
        self.bins.len()
    }

    pub fn capacity_bytes(&self) -> usize {
        self.pages.capacity() * size_of::<ParallelPage>()
            + self.lookup.capacity() * size_of::<usize>()
            + self.bins.capacity() * size_of::<Mutex<usize>>()
    }

    fn locate(&self, pointer: Pointer) -> Option<(usize, usize)> {
        if self.lookup.is_empty() {
            return None;
        }
        let address = pointer.as_ptr() as usize;
        let mask = self.lookup.len() - 1;
        let mut at = bucket(address, mask);
        loop {
            let index = self.lookup[at];
            if index == END {
                return None;
            }
            let page = &self.pages[index];
            let offset = address.wrapping_sub(page.pointer.as_ptr() as usize);
            if offset < page.size {
                return Some((index, slot_index(offset, page.stride, page.reciprocal)));
            }
            at = (at + 1) & mask;
        }
    }

    pub fn mark(&self, pointer: Pointer, descriptors: &[Descriptor]) -> Option<(usize, bool)> {
        let (index, slot) = self.locate(pointer)?;
        let page = &self.pages[index];
        let flag = page.flag(slot);
        let observed = flag.load(Ordering::Relaxed);
        if observed & LIVE == 0 {
            return None;
        }
        let offset = pointer.as_ptr() as usize - page.pointer(slot).as_ptr() as usize;
        if offset >= page.bytes(slot, descriptors).max(1) {
            return None;
        }
        let fresh = observed & MARK == 0 && flag.fetch_or(MARK, Ordering::Relaxed) & MARK == 0;
        Some(((index << self.slot_bits) | slot, fresh))
    }

    #[inline]
    pub fn allocate(
        &self,
        kind: usize,
        space: &Space,
        bytes: usize,
        major: bool,
        labs: &mut [LocalAllocation],
    ) -> (Pointer, usize) {
        let bin = kind * CLASSES
            + if space.fixed_size.is_some() {
                0
            } else {
                size_class(
                    ((bytes.max(1) + space.alignment - 1) & !(space.alignment - 1))
                        .max(size_of::<usize>()),
                )
            };
        self.allocate_bin(bin, bytes, major, &mut labs[bin])
    }

    #[inline]
    pub fn allocate_fixed<T, const KIND: usize>(
        &self,
        major: bool,
        labs: &mut [LocalAllocation],
    ) -> (Pointer, usize) {
        self.allocate_bin(
            KIND * CLASSES,
            size_of::<T>(),
            major,
            &mut labs[KIND * CLASSES],
        )
    }

    #[inline]
    fn allocate_bin(
        &self,
        bin: usize,
        bytes: usize,
        major: bool,
        local: &mut LocalAllocation,
    ) -> (Pointer, usize) {
        if local.remaining == 0 {
            *local = self.refill(bin);
        }
        let page = &self.pages[local.page];
        let slot = local.take(page);
        if page.variable {
            // SAFETY: the slot belongs exclusively to this worker. No live
            // pointer to it is published until after copying the payload.
            unsafe { page.lengths.add(slot).write(bytes) };
        }
        page.flag(slot)
            .store(LIVE | if major { MARK } else { 0 }, Ordering::Relaxed);
        (page.pointer(slot), (local.page << self.slot_bits) | slot)
    }

    #[inline(never)]
    fn refill(&self, bin: usize) -> LocalAllocation {
        let mut head = self.bins[bin].lock().unwrap();
        assert_ne!(
            *head, END,
            "preflight includes all objects and partial worker leases"
        );
        let index = *head;
        let page = &self.pages[index];
        // SAFETY: only the mutex for this page's size class accesses its cursor.
        let allocation = unsafe { &mut *page.allocation.get() };
        let linked = allocation.free != END;
        let (next, remaining) = if linked {
            let first = allocation.free;
            let mut last = first;
            let mut count = 0;
            while allocation.free != END && count < LAB {
                last = allocation.free;
                // SAFETY: the bin lock exclusively owns the shared free list.
                allocation.free =
                    unsafe { page.pointer(last).cast::<usize>().as_ptr().read_unaligned() };
                count += 1;
            }
            // SAFETY: these slots now belong exclusively to the returned lease.
            // Terminate its private chain before releasing the bin lock.
            unsafe {
                page.pointer(last)
                    .cast::<usize>()
                    .as_ptr()
                    .write_unaligned(END)
            };
            (first, count)
        } else {
            let first = allocation.bump;
            let end = (first + LAB).min(page.slots);
            allocation.bump = end;
            (first, end - first)
        };
        assert!(remaining > 0);
        allocation.free_count -= remaining;
        if allocation.free_count == 0 {
            *head = allocation.next_free;
        }
        LocalAllocation {
            page: index,
            next,
            remaining,
            linked,
        }
    }

    /// Called only after all graph workers have joined, once for each worker.
    pub fn return_unused(&mut self, labs: &mut [LocalAllocation]) {
        for local in labs {
            if local.remaining == 0 {
                continue;
            }
            // All remaining links must be read before returning their slots to
            // the shared list, which overwrites those same payload words.
            let page = &self.pages[local.page];
            let mut slots = [0; LAB];
            let count = local.remaining;
            for slot in &mut slots[..count] {
                *slot = local.take(page);
            }
            let page = &mut self.pages[local.page];
            let allocation = page.allocation.get_mut();
            for &slot in &slots[..count] {
                // SAFETY: this unconsumed lease never owned an initialized object.
                unsafe {
                    page.pointer
                        .as_ptr()
                        .add(slot * page.stride)
                        .cast::<usize>()
                        .write_unaligned(allocation.free)
                };
                allocation.free = slot;
                allocation.free_count += 1;
            }
        }
    }

    pub fn seed_remembered(&self, mut push: impl FnMut(usize)) {
        let mut index = self.dirty;
        while index != END {
            let page = &self.pages[index];
            for slot in 0..page.slots {
                if page.flag(slot).fetch_and(!DIRTY, Ordering::Relaxed) & DIRTY != 0 {
                    push((index << self.slot_bits) | slot);
                }
            }
            index = page.next_dirty;
        }
    }

    pub fn trace_info(&self, work: usize, descriptors: &[Descriptor]) -> (Pointer, usize, usize) {
        let page = &self.pages[work >> self.slot_bits];
        let slot = work & ((1 << self.slot_bits) - 1);
        (page.pointer(slot), page.bytes(slot, descriptors), page.kind)
    }

    pub fn finish(mut self, used: usize, count: usize) {
        self.heap.used += used;
        self.heap.count += count;
        self.heap.dirty = END;
        for (page, parallel) in self.heap.pages.iter_mut().zip(&mut self.pages) {
            let allocation = parallel.allocation.get_mut();
            page.free = allocation.free;
            page.bump = allocation.bump;
            page.free_count = allocation.free_count;
            page.dirty = false;
        }
        // Bin lists are rebuilt after reclamation, before the mutator resumes.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn concurrent_swept_leases_return_unused_slots_without_loss_or_overlap() {
        const OBJECTS: usize = 2048;
        const BYTES: usize = size_of::<[usize; 2]>();
        unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
        for fixed in [true, false] {
            let descriptors = [Descriptor {
                name: "lease test",
                fixed_size: fixed.then_some(BYTES),
                alignment: align_of::<usize>(),
                trace,
                drop: None,
            }];
            let spaces = [Space::new(&descriptors[0]).unwrap()];
            let extent = spaces[0].extent_known(BYTES);
            let mut heap = OldHeap::new(1, 4096).unwrap();
            let bin = OldHeap::bin(0, &spaces[0], BYTES).unwrap().0;
            let mut demands = vec![0; CLASSES];
            demands[bin] = OBJECTS;
            heap.reserve(&demands, &spaces, 4096).unwrap();
            let mut holes = BTreeSet::new();
            let mut survivors = Vec::new();
            for i in 0..OBJECTS {
                // Noncontiguous holes, including partial batches at page ends.
                let keep = i % 3 == 0;
                let (p, _) = heap.allocate(0, &spaces[0], BYTES, keep);
                // SAFETY: unique, suitably sized and aligned allocation.
                unsafe { p.cast::<[usize; 2]>().as_ptr().write([i, !i]) };
                if keep {
                    survivors.push((p, i));
                } else {
                    holes.insert(p.as_ptr() as usize);
                }
            }
            heap.sweep(&descriptors, &spaces);
            let capacity = heap.capacity;
            let mut parallel = ParallelOld::new(&mut heap).unwrap();
            let counts = [1, 17, 31, 63, 64, 65, 127, 128];
            let mut locals: Vec<_> = counts
                .iter()
                .map(|_| Mutex::new(LocalAllocation::default()))
                .collect();
            let results: Vec<_> = counts.iter().map(|_| Mutex::new(Vec::new())).collect();
            let mut pool = super::super::super::pool::Pool::new(counts.len()).unwrap();
            pool.run(|worker| {
                let mut local = locals[worker].lock().unwrap();
                let mut result = results[worker].lock().unwrap();
                for _ in 0..counts[worker] {
                    let (p, _) = parallel.allocate_bin(bin, BYTES, false, &mut local);
                    let address = p.as_ptr() as usize;
                    // SAFETY: this worker exclusively owns the leased slot.
                    // Overwriting both words catches stale free-list traversal.
                    unsafe { p.cast::<[usize; 2]>().as_ptr().write([address, !address]) };
                    result.push(address);
                }
            });
            for local in &mut locals {
                parallel.return_unused(std::slice::from_mut(local.get_mut().unwrap()));
            }
            let mut allocated = BTreeSet::new();
            for result in results {
                for address in result.into_inner().unwrap() {
                    assert!(holes.contains(&address));
                    assert!(allocated.insert(address), "overlapping worker leases");
                }
            }
            let first_count = allocated.len();
            parallel.finish(first_count * extent, first_count);
            // Collection completion rebuilds bin lists, making returned leases
            // from previously exhausted pages available to the next collection.
            heap.release_empty_pages();
            let parallel = ParallelOld::new(&mut heap).unwrap();
            let mut local = LocalAllocation::default();
            while allocated.len() < holes.len() {
                let (p, _) = parallel.allocate_bin(bin, BYTES, false, &mut local);
                let address = p.as_ptr() as usize;
                assert!(holes.contains(&address));
                assert!(
                    allocated.insert(address),
                    "returned lease overlaps a live slot"
                );
                // SAFETY: unique allocation from the remaining free slots.
                unsafe { p.cast::<[usize; 2]>().as_ptr().write([address, !address]) };
            }
            assert_eq!(local.remaining, 0);
            // SAFETY: no worker phase is active; all metadata reads are joined.
            assert!(parallel
                .pages
                .iter()
                .all(|p| unsafe { (*p.allocation.get()).free_count == 0 }));
            let rest = holes.len() - first_count;
            parallel.finish(rest * extent, rest);
            assert_eq!(heap.capacity, capacity);
            assert_eq!(heap.count, OBJECTS);
            assert_eq!(heap.used, OBJECTS * extent);
            for (p, i) in survivors {
                // SAFETY: retained objects must not have been leased or moved.
                assert_eq!(unsafe { p.cast::<[usize; 2]>().as_ptr().read() }, [i, !i]);
            }
            for address in allocated {
                // SAFETY: all addresses are distinct live allocations above.
                assert_eq!(
                    unsafe { (address as *const [usize; 2]).read() },
                    [address, !address]
                );
            }
        }
    }
}
