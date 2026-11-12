//! Compacting, segregated slabs. Fixed kinds have one size class; variable
//! kinds use four size classes per doubling. Reclaimed slots store their own
//! next index; never-used slots are allocated with a bump cursor.
//! A byte per slot holds liveness, marking and remembered-set bits; only
//! variable kinds need a separate logical length per slot.

use super::*;

#[cfg(not(target_family = "wasm"))]
mod parallel;
#[cfg(not(target_family = "wasm"))]
pub(super) use parallel::{LocalAllocation, ParallelOld};
#[cfg(not(target_family = "wasm"))]
mod maintenance;
#[cfg(not(target_family = "wasm"))]
pub(super) use maintenance::Maintenance;

const LIVE: u8 = 1;
const MARK: u8 = 2;
const DIRTY: u8 = 4;
const END: usize = usize::MAX;
const CLASSES: usize = usize::BITS as usize * 4;
// Index virtual-address granules, not individual objects. Slabs can be
// unaligned or span many granules; each overlap gets a table entry. Lookup
// validates the allocation range, including collisions and shared granules.
const GRANULE_SHIFT: u32 = 16;

// Most environments, attribute leaves and pointer arrays are small. Cache
// their class arithmetic; alignment has already rounded sizes to a word.
const SMALL_LIMIT: usize = 4096;
const SMALL_CLASSES: [u8; SMALL_LIMIT / size_of::<usize>() + 1] = {
    let mut classes = [0; SMALL_LIMIT / size_of::<usize>() + 1];
    let mut word = 2;
    while word < classes.len() {
        let extent = word * size_of::<usize>();
        let exponent = (extent - 1).ilog2() as usize;
        let base = 1usize << exponent;
        classes[word] = (exponent * 4 + ((extent - 1 - base) >> (exponent - 2))) as u8;
        word += 1;
    }
    classes
};

#[inline]
fn size_class(extent: usize) -> usize {
    if extent <= SMALL_LIMIT {
        SMALL_CLASSES[extent / size_of::<usize>()] as usize
    } else {
        let exponent = (extent - 1).ilog2() as usize;
        let base = 1usize << exponent;
        exponent * 4 + ((extent - 1 - base) >> (exponent - 2))
    }
}

fn bucket(address: usize, mask: usize) -> usize {
    let key = address >> GRANULE_SHIFT;
    {
        let mixed = (key ^ (key >> 16)).wrapping_mul(0x9e3779b9);
        // Fold after multiplication too: low-bit masking alone aliases
        // chunks separated by 1 GiB in a small nursery address table.
        (mixed ^ (mixed >> (usize::BITS / 2))) & mask
    }
}

fn index_page(table: &mut [usize], index: usize, page: &Page) {
    let mask = table.len() - 1;
    let start = page.chunk.pointer.as_ptr() as usize;
    let first = start >> GRANULE_SHIFT;
    let last = (start + page.chunk.layout.size() - 1) >> GRANULE_SHIFT;
    for granule in first..=last {
        let mut slot = bucket(granule << GRANULE_SHIFT, mask);
        while table[slot] != END {
            slot = (slot + 1) & mask;
        }
        table[slot] = index;
    }
}

struct Bin {
    head: usize,
    tail: usize,
    free: usize,
}

struct Page {
    chunk: Chunk,
    kind: usize,
    bin: usize,
    stride: usize,
    reciprocal: u64,
    flags: Vec<u8>,
    lengths: Vec<usize>,
    free: usize,
    // Untouched suffix: free slots here need no payload links or page faults.
    bump: usize,
    free_count: usize,
    next_free: usize,
    next_dirty: usize,
    dirty: bool,
}

impl Page {
    fn pointer(&self, slot: usize) -> Pointer {
        // SAFETY: callers use slot indices within this page.
        unsafe { Pointer::new_unchecked(self.chunk.pointer.as_ptr().add(slot * self.stride)) }
    }

    fn bytes(&self, slot: usize, descriptors: &[Descriptor]) -> usize {
        descriptors[self.kind]
            .fixed_size
            .unwrap_or_else(|| self.lengths[slot])
    }
}

/// Scratch for an in-place sliding compaction. Each size class is packed
/// toward its earliest pages in allocation order, never over an unprocessed
/// live source. This needs forwarding metadata, not a second old heap.
pub(super) struct Compaction {
    offsets: Vec<usize>,
    targets: Vec<Option<Pointer>>,
    cursors: Vec<(usize, usize)>,
    next: Vec<usize>,
}

impl Compaction {
    pub fn capacity(&self) -> usize {
        self.offsets.capacity() * size_of::<usize>()
            + self.targets.capacity() * size_of::<Option<Pointer>>()
            + self.cursors.capacity() * size_of::<(usize, usize)>()
            + self.next.capacity() * size_of::<usize>()
    }
}

pub(super) struct OldHeap {
    pages: Vec<Page>,
    // Open-addressed page index: one entry per virtual-address granule/slab
    // overlap, rather than per object. Always less than half full.
    lookup: Vec<usize>,
    bins: Vec<Bin>,
    dirty: usize,
    slot_bits: u32,
    pub used: usize,
    pub capacity: usize,
    pub count: usize,
}

impl OldHeap {
    #[cfg(not(target_family = "wasm"))]
    pub fn worker_slack(
        demands: &mut [usize],
        spaces: &[Space],
        chunk_bytes: usize,
        workers: usize,
    ) -> Result<()> {
        for (bin, demand) in demands.iter_mut().enumerate() {
            if *demand == 0 {
                continue;
            }
            let space = &spaces[bin / CLASSES];
            let stride = if let Some(bytes) = space.fixed_size {
                Self::bin(bin / CLASSES, space, bytes)?.1
            } else {
                Self::class_stride(bin % CLASSES)?
            };
            let lease = (chunk_bytes.clamp(64, 64 * 1024) / stride).clamp(1, parallel::LAB);
            // Large single-slot slabs have no partial lease, hence no slack.
            *demand = demand
                .checked_add(workers * (lease - 1))
                .ok_or(Error::AllocationTooLarge)?;
        }
        Ok(())
    }
    #[cfg(not(target_family = "wasm"))]
    pub fn dirty_count(&self) -> usize {
        let mut count = 0;
        let mut page = self.dirty;
        while page != END {
            count += self.pages[page]
                .flags
                .iter()
                .filter(|&&f| f & DIRTY != 0)
                .count();
            page = self.pages[page].next_dirty;
        }
        count
    }
    pub fn new(kinds: usize, chunk_bytes: usize) -> Result<Self> {
        let mut bins = reserved(
            kinds
                .checked_mul(CLASSES)
                .ok_or(Error::AllocationTooLarge)?,
        )?;
        bins.resize_with(kinds * CLASSES, || Bin {
            head: END,
            tail: END,
            free: 0,
        });
        Ok(Self {
            pages: Vec::new(),
            lookup: Vec::new(),
            bins,
            dirty: END,
            slot_bits: (chunk_bytes.clamp(64, 64 * 1024) / size_of::<usize>())
                .next_power_of_two()
                .trailing_zeros(),
            used: 0,
            capacity: 0,
            count: 0,
        })
    }

    #[inline]
    pub fn bin(kind: usize, space: &Space, bytes: usize) -> Result<(usize, usize)> {
        // Variable lengths live in side metadata here. The nursery's size
        // prefix is not part of an old payload (and would double the class
        // for payloads whose size is already a power of two).
        let extent = if space.fixed_size.is_some() {
            space.extent(bytes)?
        } else {
            round_up(bytes.max(1), space.alignment)?
        }
        .max(size_of::<usize>());
        if space.fixed_size.is_some() {
            Ok((kind * CLASSES, round_up(extent, space.alignment)?))
        } else {
            if extent <= size_of::<usize>() {
                return Ok((kind * CLASSES, size_of::<usize>()));
            }
            // Quarter-power steps cap rounding slack at 25%, instead of
            // almost 100% just above a power of two. Extent is already aligned;
            // each step either divides that alignment or is a multiple of it.
            let class = size_class(extent);
            Ok((kind * CLASSES + class, Self::class_stride(class)?))
        }
    }

    fn class_stride(class: usize) -> Result<usize> {
        if class == 0 {
            return Ok(size_of::<usize>());
        }
        let base = 1usize << (class / 4);
        base.checked_add((class % 4 + 1) * (base / 4))
            .ok_or(Error::AllocationTooLarge)
    }

    pub fn demands(&self) -> Result<Vec<usize>> {
        let mut demands = reserved(self.bins.len())?;
        demands.resize(self.bins.len(), 0);
        Ok(demands)
    }

    /// Pre-reserve slots for every young object, even if none dies. All new
    /// pages remain local until every reservation succeeds, so failure leaves
    /// both generations and their address index unchanged.
    pub fn reserve(
        &mut self,
        demands: &[usize],
        spaces: &[Space],
        chunk_bytes: usize,
    ) -> Result<()> {
        self.reserve_with_pool(
            demands,
            spaces,
            chunk_bytes,
            #[cfg(not(target_family = "wasm"))]
            None,
        )
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn reserve_parallel(
        &mut self,
        demands: &[usize],
        spaces: &[Space],
        chunk_bytes: usize,
        pool: &mut super::pool::Pool,
    ) -> Result<()> {
        self.reserve_with_pool(demands, spaces, chunk_bytes, Some(pool))
    }

    fn reserve_with_pool(
        &mut self,
        demands: &[usize],
        spaces: &[Space],
        chunk_bytes: usize,
        #[cfg(not(target_family = "wasm"))] pool: Option<&mut super::pool::Pool>,
    ) -> Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let added = if let Some(pool) = pool {
            parallel::reserve_pages(self, demands, spaces, chunk_bytes, pool)?
        } else {
            self.reserve_pages_range(demands, spaces, chunk_bytes, 0, demands.len())?
        };
        #[cfg(target_family = "wasm")]
        let added = self.reserve_pages_range(demands, spaces, chunk_bytes, 0, demands.len())?;
        let pages = self
            .pages
            .len()
            .checked_add(added.len())
            .ok_or(Error::AllocationTooLarge)?;
        if pages != 0 && pages - 1 > (usize::MAX >> self.slot_bits) {
            return Err(Error::AllocationTooLarge);
        }
        #[cfg(test)]
        reservation_checkpoint()?;
        self.pages
            .try_reserve(added.len())
            .map_err(|_| Error::OutOfMemory)?;
        let entries: usize = self
            .pages
            .iter()
            .chain(&added)
            .map(|page| {
                let start = page.chunk.pointer.as_ptr() as usize;
                ((start + page.chunk.layout.size() - 1) >> GRANULE_SHIFT) - (start >> GRANULE_SHIFT)
                    + 1
            })
            .sum();
        let capacity = entries
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .and_then(usize::checked_next_power_of_two)
            .ok_or(Error::AllocationTooLarge)?;
        let mut lookup = reserved(capacity)?;
        lookup.resize(capacity, END);
        for (index, page) in self.pages.iter().chain(&added).enumerate() {
            index_page(&mut lookup, index, page);
        }
        for mut page in added {
            let index = self.pages.len();
            let bin = &mut self.bins[page.bin];
            // Fill older partial slabs first, then new slabs in allocation
            // order. Reverse-order promotion leaves gaps before full slabs
            // and forces needless movement during the next compaction.
            if bin.tail == END {
                bin.head = index;
            } else {
                self.pages[bin.tail].next_free = index;
            }
            bin.tail = index;
            page.next_free = END;
            bin.free += page.free_count;
            self.capacity += page.chunk.layout.size();
            self.pages.push(page);
        }
        self.lookup = lookup;
        Ok(())
    }

    fn reserve_pages_range(
        &self,
        demands: &[usize],
        spaces: &[Space],
        chunk_bytes: usize,
        first: usize,
        end: usize,
    ) -> Result<Vec<Page>> {
        let mut added = reserved(0)?;
        for (bin, &demand) in demands.iter().enumerate().take(end).skip(first) {
            let mut needed = demand.saturating_sub(self.bins[bin].free);
            if needed == 0 {
                continue;
            }
            let kind = bin / CLASSES;
            let space = &spaces[kind];
            let stride = if let Some(bytes) = space.fixed_size {
                Self::bin(kind, space, bytes)?.1
            } else {
                Self::class_stride(bin % CLASSES)?
            };
            // Bound slab size so compaction can return empty pages even when
            // only a handful of a formerly large graph remains alive.
            let slots = (chunk_bytes.clamp(64, 64 * 1024) / stride).max(1);
            let page_count = needed.div_ceil(slots);
            #[cfg(test)]
            reservation_checkpoint()?;
            added
                .try_reserve(page_count)
                .map_err(|_| Error::OutOfMemory)?;
            while needed != 0 {
                let bytes = slots.checked_mul(stride).ok_or(Error::AllocationTooLarge)?;
                let alignment = space.alignment.max(if bytes >= 1 << 20 { 4096 } else { 1 });
                let layout = Layout::from_size_align(bytes, alignment)
                    .map_err(|_| Error::AllocationTooLarge)?;
                let mut flags = reserved(slots)?;
                flags.resize(slots, 0);
                let mut lengths = reserved(if space.fixed_size.is_none() { slots } else { 0 })?;
                if space.fixed_size.is_none() {
                    lengths.resize(slots, 0);
                }
                #[cfg(test)]
                reservation_checkpoint()?;
                // SAFETY: layout is nonempty and validated.
                let pointer = Pointer::new(unsafe { alloc(layout) }).ok_or(Error::OutOfMemory)?;
                let page = Page {
                    chunk: Chunk {
                        pointer,
                        layout,
                        used: 0,
                        first_variable: 0,
                    },
                    kind,
                    bin,
                    stride,
                    reciprocal: slot_reciprocal(stride, bytes),
                    flags,
                    lengths,
                    free: END,
                    bump: 0,
                    free_count: slots,
                    next_free: END,
                    next_dirty: END,
                    dirty: false,
                };
                added.push(page);
                needed = needed.saturating_sub(slots);
            }
        }
        Ok(added)
    }

    pub fn allocate(
        &mut self,
        kind: usize,
        space: &Space,
        bytes: usize,
        major: bool,
    ) -> (Pointer, usize) {
        // All payloads were validated and their destinations reserved before
        // tracing. Allocation needs the bin index, not layout validation again.
        let bin = kind * CLASSES
            + if space.fixed_size.is_some() {
                0
            } else {
                let extent = ((bytes.max(1) + space.alignment - 1) & !(space.alignment - 1))
                    .max(size_of::<usize>());
                size_class(extent)
            };
        self.allocate_bin(bin, space.extent_known(bytes), bytes, None, major)
    }

    #[inline(always)]
    pub fn allocate_fixed<T, const KIND: usize>(&mut self, major: bool) -> (Pointer, usize) {
        let extent = size_of::<T>().max(1).next_multiple_of(align_of::<T>());
        let stride = extent
            .max(size_of::<usize>())
            .next_multiple_of(align_of::<T>());
        self.allocate_bin(KIND * CLASSES, extent, size_of::<T>(), Some(stride), major)
    }

    #[inline(always)]
    fn allocate_bin(
        &mut self,
        bin: usize,
        nursery_extent: usize,
        bytes: usize,
        fixed_stride: Option<usize>,
        major: bool,
    ) -> (Pointer, usize) {
        let bin = &mut self.bins[bin];
        let index = bin.head;
        let page = &mut self.pages[index];
        let slot = if page.free == END {
            let slot = page.bump;
            page.bump += 1;
            slot
        } else {
            let slot = page.free;
            // SAFETY: swept slots contain initialized links. Virgin slots
            // use the bump cursor and are never read before initialization.
            page.free = unsafe { page.pointer(slot).cast::<usize>().as_ptr().read_unaligned() };
            slot
        };
        let pointer = if let Some(stride) = fixed_stride {
            debug_assert_eq!(stride, page.stride);
            // SAFETY: pre-reserved fixed slot, with its validated stride.
            unsafe { Pointer::new_unchecked(page.chunk.pointer.as_ptr().add(slot * stride)) }
        } else {
            page.pointer(slot)
        };
        page.free_count -= 1;
        bin.free -= 1;
        if page.free_count == 0 {
            bin.head = page.next_free;
            if bin.head == END {
                bin.tail = END;
            }
        }
        page.flags[slot] = LIVE | if major { MARK } else { 0 };
        if fixed_stride.is_none() && !page.lengths.is_empty() {
            page.lengths[slot] = bytes;
        }
        self.used += nursery_extent;
        self.count += 1;
        (pointer, (index << self.slot_bits) | slot)
    }

    #[inline]
    fn locate_page(&self, pointer: Pointer) -> Option<(usize, usize)> {
        if self.lookup.is_empty() {
            return None;
        }
        let address = pointer.as_ptr() as usize;
        let mask = self.lookup.len() - 1;
        let mut slot = bucket(address, mask);
        loop {
            let index = self.lookup[slot];
            if index == END {
                return None;
            }
            let page = &self.pages[index];
            let offset = address.wrapping_sub(page.chunk.pointer.as_ptr() as usize);
            if offset < page.chunk.layout.size() {
                return Some((index, offset));
            }
            slot = (slot + 1) & mask;
        }
    }

    #[inline]
    fn locate(&self, pointer: Pointer) -> Option<(usize, usize)> {
        let (index, offset) = self.locate_page(pointer)?;
        let page = &self.pages[index];
        Some((index, slot_index(offset, page.stride, page.reciprocal)))
    }

    #[inline(always)]
    pub fn object(
        &self,
        pointer: Pointer,
        descriptors: &[Descriptor],
    ) -> Option<(usize, usize, Pointer, usize)> {
        let (index, slot) = self.locate(pointer)?;
        let page = &self.pages[index];
        if page.flags[slot] & LIVE == 0 {
            return None;
        }
        let owner = page.pointer(slot);
        let bytes = page.bytes(slot, descriptors);
        let offset = pointer.as_ptr() as usize - owner.as_ptr() as usize;
        (offset < bytes.max(1)).then_some((index, slot, owner, bytes))
    }

    pub fn mark(&mut self, pointer: Pointer, descriptors: &[Descriptor]) -> Option<(usize, bool)> {
        let (index, slot, _, _) = self.object(pointer, descriptors)?;
        let flag = &mut self.pages[index].flags[slot];
        let fresh = *flag & MARK == 0;
        *flag |= MARK;
        Some(((index << self.slot_bits) | slot, fresh))
    }

    /// Mark an exact fixed-layout payload start. The caller guarantees the
    /// same kind, size, alignment and lifetime contract as relocate_fixed.
    #[inline]
    pub unsafe fn mark_fixed<T, const KIND: usize>(
        &mut self,
        pointer: NonNull<T>,
    ) -> Option<(usize, bool)> {
        let (index, offset) = self.locate_page(pointer.cast())?;
        let page = &mut self.pages[index];
        let stride = size_of::<T>()
            .max(1)
            .next_multiple_of(align_of::<T>())
            .max(size_of::<usize>())
            .next_multiple_of(align_of::<T>());
        debug_assert_eq!(page.kind, KIND);
        debug_assert_eq!(page.stride, stride);
        debug_assert_eq!(offset % stride, 0);
        let slot = offset / stride;
        let flag = &mut page.flags[slot];
        // Strong managed pointers name initialized live objects. External
        // pointers cannot alias the storage owned by an old page.
        debug_assert_ne!(*flag & LIVE, 0);
        let fresh = *flag & MARK == 0;
        *flag |= MARK;
        Some(((index << self.slot_bits) | slot, fresh))
    }

    #[inline]
    pub fn remember(&mut self, pointer: Pointer, descriptors: &[Descriptor]) {
        let Some((index, slot)) = self.locate(pointer) else {
            return;
        };
        let page = &mut self.pages[index];
        // Repeated writes to a remembered owner need no length lookup or
        // dirty-list work. Keep this path small enough to inline at stores.
        if page.flags[slot] & (LIVE | DIRTY) != LIVE {
            return;
        }
        let offset = pointer.as_ptr() as usize - page.pointer(slot).as_ptr() as usize;
        if offset >= page.bytes(slot, descriptors).max(1) {
            return;
        }
        page.flags[slot] |= DIRTY;
        if !page.dirty {
            page.dirty = true;
            page.next_dirty = self.dirty;
            self.dirty = index;
        }
    }

    /// Trace only modified slots in dirty pages. No old-heap walk or allocation
    /// on the barrier path; links live in page metadata reserved at promotion.
    pub fn take_dirty_page(&mut self) -> Option<usize> {
        if self.dirty == END {
            return None;
        }
        let index = self.dirty;
        let page = &mut self.pages[index];
        self.dirty = page.next_dirty;
        page.dirty = false;
        Some(index)
    }

    pub fn slots(&self, page: usize) -> usize {
        self.pages[page].flags.len()
    }

    pub fn take_dirty(
        &mut self,
        page: usize,
        slot: usize,
        descriptors: &[Descriptor],
    ) -> Option<(Pointer, usize, usize)> {
        let page = &mut self.pages[page];
        if page.flags[slot] & DIRTY == 0 {
            return None;
        }
        page.flags[slot] &= !DIRTY;
        Some((page.pointer(slot), page.bytes(slot, descriptors), page.kind))
    }

    /// Work items name stable page/slot indices, so popping an object does
    /// not need another address-table lookup or division by its stride.
    pub fn trace_info(&self, work: usize, descriptors: &[Descriptor]) -> (Pointer, usize, usize) {
        let page = &self.pages[work >> self.slot_bits];
        let slot = work & ((1usize << self.slot_bits) - 1);
        (page.pointer(slot), page.bytes(slot, descriptors), page.kind)
    }

    pub fn weak(
        &self,
        pointer: Pointer,
        descriptors: &[Descriptor],
        major: bool,
    ) -> Option<Pointer> {
        let Some((index, slot)) = self.locate(pointer) else {
            return Some(pointer);
        };
        let page = &self.pages[index];
        if page.flags[slot] & LIVE == 0 || (major && page.flags[slot] & MARK == 0) {
            return None;
        }
        self.object(pointer, descriptors).map(|_| pointer)
    }

    pub fn reserve_compaction(&self) -> Result<Compaction> {
        let mut offsets = reserved(self.pages.len())?;
        let mut count = 0;
        for page in &self.pages {
            offsets.push(count);
            count += page.flags.len();
        }
        // Reserve before roots change, but initialize only if compaction is
        // useful. A dense automatic major can sweep without a forwarding map.
        let targets = reserved(count)?;
        let mut cursors = reserved(self.bins.len())?;
        cursors.resize(self.bins.len(), (END, 0));
        let mut next = reserved(self.pages.len())?;
        next.resize(self.pages.len(), END);
        for (index, page) in self.pages.iter().enumerate().rev() {
            next[index] = cursors[page.bin].0;
            cursors[page.bin].0 = index;
        }
        Ok(Compaction {
            offsets,
            targets,
            cursors,
            next,
        })
    }

    pub fn plan_compaction(&self, plan: &mut Compaction) {
        let count = self.pages.iter().map(|page| page.flags.len()).sum();
        plan.targets.resize(count, None);
        for (index, page) in self.pages.iter().enumerate() {
            for slot in 0..page.flags.len() {
                if page.flags[slot] & MARK == 0 {
                    continue;
                }
                let (target_page, target_slot) = &mut plan.cursors[page.bin];
                if *target_slot == self.pages[*target_page].flags.len() {
                    *target_page = plan.next[*target_page];
                    *target_slot = 0;
                }
                debug_assert!(
                    *target_page < index || (*target_page == index && *target_slot <= slot)
                );
                plan.targets[plan.offsets[index] + slot] =
                    Some(self.pages[*target_page].pointer(*target_slot));
                *target_slot += 1;
            }
        }
    }

    #[inline]
    pub fn compacted(
        &self,
        pointer: Pointer,
        descriptors: &[Descriptor],
        plan: &Compaction,
    ) -> Option<Pointer> {
        let Some((index, slot)) = self.locate(pointer) else {
            return Some(pointer);
        };
        let page = &self.pages[index];
        if page.flags[slot] & LIVE == 0 {
            return None;
        }
        let offset = pointer.as_ptr() as usize - page.pointer(slot).as_ptr() as usize;
        if offset >= page.bytes(slot, descriptors).max(1) {
            return None;
        }
        plan.targets[plan.offsets[index] + slot].map(|target| {
            // SAFETY: same logical object size and interior offset.
            unsafe { Pointer::new_unchecked(target.as_ptr().add(offset)) }
        })
    }

    pub fn pages(&self) -> usize {
        self.pages.len()
    }

    pub fn marked_object(
        &self,
        page: usize,
        slot: usize,
        descriptors: &[Descriptor],
    ) -> Option<(Pointer, usize, usize)> {
        let page = &self.pages[page];
        (page.flags[slot] & MARK != 0)
            .then(|| (page.pointer(slot), page.bytes(slot, descriptors), page.kind))
    }

    pub fn should_compact(&self) -> bool {
        let mut occupied = 0;
        let mut marked = 0;
        for page in &self.pages {
            occupied += (page.flags.len() - page.free_count) * page.stride;
            marked += page.flags.iter().filter(|&&flag| flag & MARK != 0).count() * page.stride;
        }
        occupied - marked > occupied / 4
    }

    /// Reclaim dead slots without changing live addresses. Later promotions
    /// fill the holes before reserving new slabs. Completely dead pages are
    /// returned immediately, just as after a compacting major.
    pub fn sweep(&mut self, descriptors: &[Descriptor], spaces: &[Space]) {
        self.dirty = END;
        for page in &mut self.pages {
            page.dirty = false;
            page.free = END;
            page.free_count = page.flags.len() - page.bump;
            for slot in (0..page.bump).rev() {
                let flag = page.flags[slot];
                if flag & MARK != 0 {
                    page.flags[slot] = LIVE;
                    continue;
                }
                if flag & LIVE != 0 {
                    let bytes = page.bytes(slot, descriptors);
                    if let Some(drop) = descriptors[page.kind].drop {
                        // SAFETY: unmarked initialized owner, dropped once.
                        unsafe { drop(page.pointer(slot), bytes) };
                    }
                    self.used -= spaces[page.kind].extent_known(bytes);
                    self.count -= 1;
                }
                page.flags[slot] = 0;
                // SAFETY: free slots no longer own a payload, and every
                // stride fits a word (possibly with weaker alignment).
                unsafe {
                    page.pointer(slot)
                        .cast::<usize>()
                        .as_ptr()
                        .write_unaligned(page.free);
                }
                page.free = slot;
                page.free_count += 1;
            }
        }
        self.release_empty_pages();
    }

    /// Edges and roots already name their final destinations. Destroy dead
    /// owners before any movement can overwrite them, then slide survivors.
    pub fn compact(
        &mut self,
        descriptors: &[Descriptor],
        spaces: &[Space],
        plan: &Compaction,
    ) -> usize {
        self.dirty = END;
        for page in &mut self.pages {
            page.dirty = false;
            for slot in 0..page.flags.len() {
                let flag = page.flags[slot];
                if flag & LIVE == 0 || flag & MARK != 0 {
                    continue;
                }
                let bytes = page.bytes(slot, descriptors);
                if let Some(drop) = descriptors[page.kind].drop {
                    // SAFETY: unreachable initialized owner, dropped once.
                    unsafe { drop(page.pointer(slot), bytes) };
                }
                self.used -= spaces[page.kind].extent_known(bytes);
                self.count -= 1;
                page.flags[slot] = 0;
            }
        }
        let mut moved = 0;
        for index in 0..self.pages.len() {
            for slot in 0..self.pages[index].flags.len() {
                let Some(target) = plan.targets[plan.offsets[index] + slot] else {
                    continue;
                };
                let page = &self.pages[index];
                let source = page.pointer(slot);
                let bytes = page.bytes(slot, descriptors);
                if source != target {
                    // SAFETY: the plan packs toward earlier slots of the same
                    // class. Every overwritten live source was already moved.
                    // ptr::copy also permits overlapping source/destination.
                    unsafe { std::ptr::copy(source.as_ptr(), target.as_ptr(), bytes) };
                    moved += spaces[page.kind].extent_known(bytes);
                }
                self.pages[index].flags[slot] = 0;
                let (target_page, target_slot) =
                    self.locate(target).expect("reserved compaction target");
                let page = &mut self.pages[target_page];
                page.flags[target_slot] = LIVE;
                if !page.lengths.is_empty() {
                    page.lengths[target_slot] = bytes;
                }
            }
        }
        for page in &mut self.pages {
            page.free = END;
            page.bump = page
                .flags
                .iter()
                .rposition(|&flag| flag != 0)
                .map_or(0, |slot| slot + 1);
            page.free_count = page.flags.len() - page.bump;
            for slot in (0..page.bump).rev() {
                if page.flags[slot] == 0 {
                    // SAFETY: free slots no longer contain owning payloads.
                    unsafe {
                        page.pointer(slot)
                            .cast::<usize>()
                            .as_ptr()
                            .write_unaligned(page.free)
                    };
                    page.free = slot;
                    page.free_count += 1;
                }
            }
        }
        self.release_empty_pages();
        moved
    }

    /// Runs after tracing/weak repair, when no scratch entry still needs page
    /// indices. Uses page counters only, never scans old payloads or slots.
    pub fn release_empty_pages(&mut self) {
        self.pages
            .retain(|page| page.free_count != page.flags.len());
        // Page indices can change only after all tracing and weak repair ends.
        self.lookup.fill(END);
        for bin in &mut self.bins {
            bin.head = END;
            bin.tail = END;
            bin.free = 0;
        }
        self.capacity = 0;
        for index in 0..self.pages.len() {
            let page = &self.pages[index];
            self.capacity += page.chunk.layout.size();
            index_page(&mut self.lookup, index, page);
            if page.free_count != 0 {
                let bin = &mut self.bins[page.bin];
                bin.free += page.free_count;
                if bin.tail == END {
                    bin.head = index;
                } else {
                    self.pages[bin.tail].next_free = index;
                }
                self.pages[index].next_free = END;
                bin.tail = index;
            }
        }
    }

    pub fn usage(&self, kind: usize, space: &Space, descriptors: &[Descriptor]) -> (usize, usize) {
        let mut used = 0;
        let mut capacity = 0;
        for page in &self.pages {
            if page.kind != kind {
                continue;
            }
            capacity += page.chunk.layout.size();
            for slot in 0..page.flags.len() {
                if page.flags[slot] & LIVE != 0 {
                    used += space.extent_known(page.bytes(slot, descriptors));
                }
            }
        }
        (used, capacity)
    }

    pub fn metadata_capacity(&self) -> usize {
        self.pages.capacity() * size_of::<Page>()
            + self.lookup.capacity() * size_of::<usize>()
            + self.bins.capacity() * size_of::<Bin>()
            + self
                .pages
                .iter()
                .map(|p| p.flags.capacity() + p.lengths.capacity() * size_of::<usize>())
                .sum::<usize>()
    }

    pub fn drop_objects(&self, descriptors: &[Descriptor]) {
        for page in &self.pages {
            if let Some(drop) = descriptors[page.kind].drop {
                for slot in 0..page.flags.len() {
                    if page.flags[slot] & LIVE != 0 {
                        // SAFETY: remaining payloads own their resources once.
                        unsafe { drop(page.pointer(slot), page.bytes(slot, descriptors)) };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_classes_preserve_alignment_and_bound_rounding() {
        unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
        for alignment in [1, 4, 8, 16, 64, 4096] {
            let space = Space::new(&Descriptor {
                name: "variable",
                fixed_size: None,
                alignment,
                trace,
                drop: None,
            })
            .unwrap();
            for bytes in 0..16_385 {
                let extent = round_up(bytes.max(1), space.alignment)
                    .unwrap()
                    .max(size_of::<usize>());
                let (bin, stride) = OldHeap::bin(0, &space, bytes).unwrap();
                assert_eq!(stride, OldHeap::class_stride(bin).unwrap());
                assert_eq!(stride % space.alignment, 0);
                assert!(stride >= extent && stride - extent <= extent / 4);
            }
        }
    }
}
