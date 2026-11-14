//! Independent slab sweeps and size-class-ordered sliding compaction.
//! Scratch is reserved before roots move. Non-Send owned resources are dropped
//! on the caller before workers reclaim or overwrite their storage.
use super::super::pool::Pool;
use super::*;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

struct View {
    pointer: Pointer,
    flags: *mut u8,
    lengths: *mut usize,
    fixed: Option<usize>,
    extent: Option<usize>,
    prefix: usize,
    alignment: usize,
    stride: usize,
    slots: usize,
    bump: usize,
}
impl View {
    fn pointer(&self, slot: usize) -> Pointer {
        // SAFETY: jobs iterate only this page's validated slot bounds.
        unsafe { Pointer::new_unchecked(self.pointer.as_ptr().add(slot * self.stride)) }
    }
    fn flag(&self, slot: usize) -> u8 {
        // SAFETY: each reclamation job owns a whole size class/page. During
        // planning only reads occur; graph workers have already joined.
        unsafe { self.flags.add(slot).read() }
    }
    fn bytes(&self, slot: usize) -> usize {
        self.fixed.unwrap_or_else(|| {
            // SAFETY: only initialized live/source slots have their length read.
            unsafe { self.lengths.add(slot).read() }
        })
    }
    fn extent(&self, bytes: usize) -> usize {
        self.extent.unwrap_or_else(|| {
            (bytes.max(1) + self.prefix + self.alignment - 1) & !(self.alignment - 1)
        })
    }
    fn set_flag(&self, slot: usize, flag: u8) {
        // SAFETY: the caller has exclusive ownership of this page's size class.
        unsafe { self.flags.add(slot).write(flag) };
    }
}
#[derive(Default)]
struct Reclaimed {
    used: usize,
    count: usize,
    moved: usize,
    free: usize,
    bump: usize,
    free_count: usize,
}

pub(in super::super) struct Maintenance {
    pages: Vec<View>,
    results: Vec<UnsafeCell<Reclaimed>>,
    first: Vec<usize>,
    next: Vec<usize>,
    bins: Vec<usize>,
    job: AtomicUsize,
}
// SAFETY: metadata is immutable during jobs. Atomic job assignment grants each
// page (sweep) or whole size class (compaction) to exactly one worker, including
// its raw flags/lengths/payload and result cell. No reference to Page is shared
// with these raw writes. All jobs join before the owning OldHeap is accessed.
unsafe impl Sync for Maintenance {}

impl Maintenance {
    pub fn reserve(heap: &OldHeap) -> Result<Self> {
        let mut results = reserved(heap.pages.len())?;
        results.resize_with(heap.pages.len(), || UnsafeCell::new(Reclaimed::default()));
        let mut first = reserved(heap.bins.len())?;
        first.resize(heap.bins.len(), END);
        let mut next = reserved(heap.pages.len())?;
        next.resize(heap.pages.len(), END);
        let mut bins = reserved(heap.bins.len())?;
        for (i, page) in heap.pages.iter().enumerate().rev() {
            next[i] = first[page.bin];
            first[page.bin] = i;
        }
        for (bin, &page) in first.iter().enumerate() {
            if page != END {
                bins.push(bin);
            }
        }
        Ok(Self {
            pages: reserved(heap.pages.len())?,
            results,
            first,
            next,
            bins,
            job: AtomicUsize::new(0),
        })
    }
    pub fn capacity_bytes(&self) -> usize {
        self.pages.capacity() * size_of::<View>()
            + self.results.capacity() * size_of::<UnsafeCell<Reclaimed>>()
            + (self.first.capacity() + self.next.capacity() + self.bins.capacity())
                * size_of::<usize>()
    }
    pub fn prepare(&mut self, heap: &mut OldHeap, spaces: &[Space]) {
        assert!(heap.pages.len() <= self.pages.capacity());
        self.pages.clear();
        for page in &mut heap.pages {
            let space = &spaces[page.kind];
            self.pages.push(View {
                pointer: page.chunk.pointer,
                flags: page.flags.as_mut_ptr(),
                lengths: page.lengths.as_mut_ptr(),
                fixed: space.fixed_size,
                extent: space.fixed_extent,
                prefix: space.prefix,
                alignment: space.alignment,
                stride: page.stride,
                slots: page.flags.len(),
                bump: page.bump,
            });
        }
    }
    fn pages(&self, mut f: impl FnMut(usize)) {
        loop {
            let first = self.job.fetch_add(16, Ordering::Relaxed);
            if first >= self.pages.len() {
                return;
            }
            for i in first..(first + 16).min(self.pages.len()) {
                f(i);
            }
        }
    }
    fn bins(&self, mut f: impl FnMut(usize)) {
        loop {
            let index = self.job.fetch_add(1, Ordering::Relaxed);
            let Some(&bin) = self.bins.get(index) else {
                return;
            };
            f(bin);
        }
    }
    pub fn should_compact(&self, pool: &mut Pool) -> bool {
        self.job.store(0, Ordering::Relaxed);
        let surviving = AtomicUsize::new(0);
        let marked = AtomicUsize::new(0);
        pool.run(|_| {
            let mut local_surviving = 0;
            let mut local_marked = 0;
            self.pages(|i| {
                let page = &self.pages[i];
                let live = (0..page.slots)
                    .filter(|&slot| page.flag(slot) & MARK != 0)
                    .count();
                if live != 0 {
                    local_surviving += page.bump * page.stride;
                    local_marked += live * page.stride;
                }
            });
            surviving.fetch_add(local_surviving, Ordering::Relaxed);
            marked.fetch_add(local_marked, Ordering::Relaxed);
        });
        fragmented(
            surviving.load(Ordering::Relaxed),
            marked.load(Ordering::Relaxed),
        )
    }
    pub fn plan(&self, plan: &mut Compaction, pool: &mut Pool) {
        let count: usize = self.pages.iter().map(|p| p.slots).sum();
        assert!(count <= plan.targets.capacity());
        let job = Plan {
            heap: self,
            targets: plan.targets.as_mut_ptr(),
            offsets: &plan.offsets,
        };
        self.job.store(0, Ordering::Relaxed);
        pool.run(|_| self.bins(|bin| job.bin(bin)));
        // SAFETY: each page's complete target slice was initialized exactly once.
        unsafe { plan.targets.set_len(count) };
    }
    fn drop_dead(heap: &OldHeap, descriptors: &[Descriptor]) {
        for page in &heap.pages {
            let Some(drop) = descriptors[page.kind].drop else {
                continue;
            };
            for slot in 0..page.bump {
                if page.flags[slot] & (LIVE | MARK) == LIVE {
                    // SAFETY: dead owners remain intact. All destructor calls
                    // stay on the collecting thread before workers overwrite storage.
                    unsafe { drop(page.pointer(slot), page.bytes(slot, descriptors)) };
                }
            }
        }
    }
    pub fn sweep(&mut self, heap: &mut OldHeap, descriptors: &[Descriptor], pool: &mut Pool) {
        Self::drop_dead(heap, descriptors);
        self.job.store(0, Ordering::Relaxed);
        pool.run(|_| {
            self.pages(|i| {
                let page = &self.pages[i];
                let mut result = Reclaimed {
                    free: END,
                    bump: page.bump,
                    free_count: page.slots - page.bump,
                    ..Reclaimed::default()
                };
                for slot in (0..page.bump).rev() {
                    let flag = page.flag(slot);
                    if flag & MARK != 0 {
                        page.set_flag(slot, LIVE);
                        continue;
                    }
                    if flag & LIVE != 0 {
                        result.used += page.extent(page.bytes(slot));
                        result.count += 1;
                    }
                    page.set_flag(slot, 0);
                    // SAFETY: this page belongs to the job; dead owners were already dropped.
                    unsafe {
                        page.pointer(slot)
                            .cast::<usize>()
                            .as_ptr()
                            .write_unaligned(result.free)
                    };
                    result.free = slot;
                    result.free_count += 1;
                }
                // SAFETY: exclusive page assignment grants this result cell too.
                unsafe { self.results[i].get().write(result) };
            })
        });
        self.finish(heap);
    }
    pub fn compact(
        &mut self,
        heap: &mut OldHeap,
        descriptors: &[Descriptor],
        plan: &Compaction,
        pool: &mut Pool,
    ) -> usize {
        Self::drop_dead(heap, descriptors);
        self.job.store(0, Ordering::Relaxed);
        let job = Compact {
            heap: self,
            targets: &plan.targets,
            offsets: &plan.offsets,
        };
        pool.run(|_| self.bins(|bin| job.bin(bin)));
        self.finish(heap)
    }
    fn finish(&mut self, heap: &mut OldHeap) -> usize {
        let mut moved = 0;
        heap.dirty = END;
        for (page, result) in heap.pages.iter_mut().zip(&mut self.results) {
            let result = result.get_mut();
            heap.used -= result.used;
            heap.count -= result.count;
            moved += result.moved;
            page.free = result.free;
            page.free_count = result.free_count;
            page.bump = result.bump;
            page.dirty = false;
        }
        heap.release_empty_pages();
        moved
    }
}

struct Plan<'a> {
    heap: &'a Maintenance,
    targets: *mut Option<Pointer>,
    offsets: &'a [usize],
}
// SAFETY: bins write disjoint source-indexed target slices, including None entries.
unsafe impl Sync for Plan<'_> {}
impl Plan<'_> {
    fn bin(&self, bin: usize) {
        let mut target_page = self.heap.first[bin];
        let mut target_slot = 0;
        let mut index = self.heap.first[bin];
        while index != END {
            let page = &self.heap.pages[index];
            for slot in 0..page.slots {
                let target = if page.flag(slot) & MARK == 0 {
                    None
                } else {
                    if target_slot == self.heap.pages[target_page].slots {
                        target_page = self.heap.next[target_page];
                        target_slot = 0;
                    }
                    debug_assert!(
                        target_page < index || (target_page == index && target_slot <= slot)
                    );
                    let pointer = self.heap.pages[target_page].pointer(target_slot);
                    target_slot += 1;
                    Some(pointer)
                };
                // SAFETY: this size class owns the full source-page target range.
                unsafe { self.targets.add(self.offsets[index] + slot).write(target) };
            }
            index = self.heap.next[index];
        }
    }
}

struct Compact<'a> {
    heap: &'a Maintenance,
    targets: &'a [Option<Pointer>],
    offsets: &'a [usize],
}
// SAFETY: each bin exclusively owns both its sources and destinations. Sources
// within a bin move in increasing order, preserving in-place slide dependencies.
unsafe impl Sync for Compact<'_> {}
impl Compact<'_> {
    fn bin(&self, bin: usize) {
        let mut index = self.heap.first[bin];
        // Account for all dead owners before an earlier destination overwrites one.
        while index != END {
            let page = &self.heap.pages[index];
            let mut result = Reclaimed::default();
            for slot in 0..page.slots {
                if page.flag(slot) & (LIVE | MARK) == LIVE {
                    result.used += page.extent(page.bytes(slot));
                    result.count += 1;
                    page.set_flag(slot, 0);
                }
            }
            // SAFETY: this bin owns the page result.
            unsafe { self.heap.results[index].get().write(result) };
            index = self.heap.next[index];
        }
        let mut target_page = self.heap.first[bin];
        let mut target_slot = 0;
        index = self.heap.first[bin];
        while index != END {
            let page = &self.heap.pages[index];
            let targets = &self.targets[self.offsets[index]..][..page.slots];
            for (slot, target) in targets.iter().enumerate() {
                let Some(target) = *target else {
                    continue;
                };
                if target_slot == self.heap.pages[target_page].slots {
                    target_page = self.heap.next[target_page];
                    target_slot = 0;
                }
                let destination = &self.heap.pages[target_page];
                debug_assert_eq!(target, destination.pointer(target_slot));
                let source = page.pointer(slot);
                let bytes = page.bytes(slot);
                if source != target {
                    // SAFETY: identical class/stride; ascending source order
                    // means no still-unprocessed live payload is overwritten.
                    unsafe {
                        std::ptr::copy(source.as_ptr(), target.as_ptr(), bytes);
                        (*self.heap.results[index].get()).moved += page.extent(bytes);
                    }
                }
                page.set_flag(slot, 0);
                destination.set_flag(target_slot, LIVE);
                if destination.fixed.is_none() {
                    // SAFETY: exclusive target size class and initialized logical length.
                    unsafe { destination.lengths.add(target_slot).write(bytes) };
                }
                target_slot += 1;
            }
            index = self.heap.next[index];
        }
        index = self.heap.first[bin];
        while index != END {
            let page = &self.heap.pages[index];
            // SAFETY: exclusive size class; all copies in this bin have completed.
            let result = unsafe { &mut *self.heap.results[index].get() };
            result.bump = (0..page.slots)
                .rfind(|&slot| page.flag(slot) != 0)
                .map_or(0, |slot| slot + 1);
            result.free = END;
            result.free_count = page.slots - result.bump;
            for slot in (0..result.bump).rev() {
                if page.flag(slot) == 0 {
                    // SAFETY: empty storage no longer owns a payload.
                    unsafe {
                        page.pointer(slot)
                            .cast::<usize>()
                            .as_ptr()
                            .write_unaligned(result.free)
                    };
                    result.free = slot;
                    result.free_count += 1;
                }
            }
            index = self.heap.next[index];
        }
    }
}
