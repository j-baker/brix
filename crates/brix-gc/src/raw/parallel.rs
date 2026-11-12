//! Stop-the-world parallel tracing. Claiming an owner precedes copying; release
//! publication precedes every read of its forwarding pointer. Payload callbacks
//! run once per owner, and only after its copy has been fully initialized.
use super::*;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

pub(super) struct RepairTracer<'a> {
    old: &'a OldHeap,
    descriptors: &'static [Descriptor],
    plan: &'a Compaction,
    #[cfg(feature = "trace-counts")]
    visits: &'a mut usize,
}
impl RepairTracer<'_> {
    #[inline]
    pub fn relocate(&mut self, pointer: Pointer) -> Pointer {
        #[cfg(feature = "trace-counts")]
        {
            *self.visits += 1;
        }
        self.old
            .compacted(pointer, self.descriptors, self.plan)
            .expect("strong edge was marked")
    }
}
struct Repair<'a> {
    old: &'a OldHeap,
    descriptors: &'static [Descriptor],
    plan: &'a Compaction,
    page: AtomicUsize,
    #[cfg(feature = "trace-counts")]
    visits: AtomicUsize,
}
// SAFETY: old metadata and plan are frozen. Jobs rewrite only their uniquely
// assigned source payloads; relocation consults metadata, never pointed-to data.
unsafe impl Sync for Repair<'_> {}
impl Repair<'_> {
    fn work(&self) {
        #[cfg(feature = "trace-counts")]
        let mut visits = 0;
        let mut tracer = Tracer {
            inner: TracerKind::Repair(RepairTracer {
                old: self.old,
                descriptors: self.descriptors,
                plan: self.plan,
                #[cfg(feature = "trace-counts")]
                visits: &mut visits,
            }),
        };
        loop {
            let first = self.page.fetch_add(16, Ordering::Relaxed);
            if first >= self.old.pages() {
                break;
            }
            for page in first..(first + 16).min(self.old.pages()) {
                for slot in 0..self.old.slots(page) {
                    if let Some((pointer, bytes, kind)) =
                        self.old.marked_object(page, slot, self.descriptors)
                    {
                        // SAFETY: exactly one worker repairs each live source.
                        unsafe { (self.descriptors[kind].trace)(pointer, bytes, &mut tracer) };
                    }
                }
            }
        }
        #[cfg(feature = "trace-counts")]
        self.visits.fetch_add(visits, Ordering::Relaxed);
    }
}
pub(super) fn repair(
    old: &OldHeap,
    descriptors: &'static [Descriptor],
    plan: &Compaction,
    pool: &mut pool::Pool,
    roots: &mut impl FnMut(&mut Tracer<'_>),
    #[cfg(feature = "trace-counts")] counts: &mut TraceCounts,
) {
    let _abort = pool::AbortOnUnwind;
    #[cfg(feature = "trace-counts")]
    let mut visits = 0;
    let mut tracer = Tracer {
        inner: TracerKind::Repair(RepairTracer {
            old,
            descriptors,
            plan,
            #[cfg(feature = "trace-counts")]
            visits: &mut visits,
        }),
    };
    roots(&mut tracer);
    let context = Repair {
        old,
        descriptors,
        plan,
        page: AtomicUsize::new(0),
        #[cfg(feature = "trace-counts")]
        visits: AtomicUsize::new(visits),
    };
    pool.run(|_| context.work());
    #[cfg(feature = "trace-counts")]
    {
        counts.visits += context.visits.load(Ordering::Relaxed);
    }
}

const LOCAL: usize = 128;
const BATCH: usize = 32;

struct Demand {
    bins: Vec<usize>,
    error: Option<Error>,
}
struct Prepare<'a> {
    variables: *mut VariableObject,
    ranges: &'a [ChunkRange],
    spaces: &'a [Space],
    demands: &'a [UnsafeCell<Demand>],
    range: AtomicUsize,
}
// SAFETY: nursery headers/spaces are immutable, ranges own disjoint output
// slices, and each pool worker has a unique demand accumulator.
unsafe impl Sync for Prepare<'_> {}
impl Prepare<'_> {
    fn work(&self, worker: usize) {
        // SAFETY: one invocation per worker index.
        let demand = unsafe { &mut *self.demands[worker].get() };
        loop {
            let index = self.range.fetch_add(1, Ordering::Relaxed);
            let Some(range) = self.ranges.get(index) else {
                return;
            };
            if range.fixed_extent.is_some() {
                continue;
            }
            let space = &self.spaces[range.kind];
            let used = range.end - range.pointer.as_ptr() as usize;
            let mut offset = 0;
            for local in 0..range.count {
                assert!(offset < used);
                // SAFETY: each published variable allocation has an initialized header.
                let bytes = unsafe { range.pointer.as_ptr().add(offset).cast::<usize>().read() };
                let bin = match OldHeap::bin(range.kind, space, bytes) {
                    Ok((bin, _)) => bin,
                    Err(error) => {
                        demand.error = Some(error);
                        return;
                    }
                };
                demand.bins[bin] += 1;
                // SAFETY: this range owns this pre-reserved, previously uninitialized record.
                unsafe {
                    self.variables
                        .add(range.variable_first + local)
                        .write(VariableObject {
                            offset: offset + space.prefix,
                            bytes,
                        })
                };
                offset += space.extent_known(bytes);
            }
            assert_eq!(offset, used);
        }
    }
}
pub(super) fn prepare_variables(
    variables: &mut Vec<VariableObject>,
    count: usize,
    ranges: &[ChunkRange],
    spaces: &[Space],
    demands: &mut [usize],
    pool: &mut pool::Pool,
) -> Result<()> {
    assert!(count <= variables.capacity());
    let mut locals = reserved(pool.workers())?;
    for _ in 0..pool.workers() {
        let mut bins = reserved(demands.len())?;
        bins.resize(demands.len(), 0);
        locals.push(UnsafeCell::new(Demand { bins, error: None }));
    }
    let prepare = Prepare {
        variables: variables.as_mut_ptr(),
        ranges,
        spaces,
        demands: &locals,
        range: AtomicUsize::new(0),
    };
    pool.run(|worker| prepare.work(worker));
    for local in &mut locals {
        let local = local.get_mut();
        if let Some(error) = local.error {
            return Err(error);
        }
        for (total, count) in demands.iter_mut().zip(&local.bins) {
            *total = total.checked_add(*count).ok_or(Error::AllocationTooLarge)?;
        }
    }
    // SAFETY: every range completed its unique initialized metadata slice.
    unsafe { variables.set_len(count) };
    Ok(())
}

struct Index<'a> {
    blocks: *mut VariableBlock,
    ranges: &'a [ChunkRange],
    variables: &'a [VariableObject],
    range: AtomicUsize,
}
// SAFETY: immutable variable records; disjoint bitmap slices per source range.
unsafe impl Sync for Index<'_> {}
impl Index<'_> {
    fn work(&self) {
        loop {
            let index = self.range.fetch_add(1, Ordering::Relaxed);
            let Some(range) = self.ranges.get(index) else {
                return;
            };
            if range.fixed_extent.is_some() {
                continue;
            }
            let count =
                (range.end - range.pointer.as_ptr() as usize).div_ceil(64 * align_of::<usize>());
            // SAFETY: the constructor assigns disjoint complete block ranges.
            let blocks = unsafe {
                std::slice::from_raw_parts_mut(self.blocks.add(range.variable_block_first), count)
            };
            for object in &self.variables[range.variable_first..range.variable_first + range.count]
            {
                let word = object.offset / align_of::<usize>();
                blocks[word / 64].starts |= 1u64 << (word % 64);
            }
            let mut preceding = 0;
            for block in blocks {
                block.preceding = preceding;
                preceding += block.starts.count_ones() as usize;
            }
            debug_assert_eq!(preceding, range.count);
        }
    }
}
pub(super) fn index_blocks(
    blocks: &mut [VariableBlock],
    ranges: &[ChunkRange],
    variables: &[VariableObject],
    pool: &mut pool::Pool,
) {
    let index = Index {
        blocks: blocks.as_mut_ptr(),
        ranges,
        variables,
        range: AtomicUsize::new(0),
    };
    pool.run(|_| index.work());
}

struct Queue {
    pending: Worklist,
    active: usize,
    done: bool,
}
struct Scheduler {
    queue: Mutex<Queue>,
    available: Condvar,
}
impl Scheduler {
    fn share(&self, local: &mut Vec<usize>, count: usize) {
        let mut queue = self.queue.lock().unwrap();
        for _ in 0..count {
            queue.pending.push(local.pop().unwrap());
        }
        self.available.notify_all();
    }
    fn take(&self, local: &mut Vec<usize>, active: &mut bool) -> bool {
        let mut queue = self.queue.lock().unwrap();
        if *active {
            queue.active -= 1;
            *active = false;
        }
        loop {
            if queue.done {
                return false;
            }
            if let Some(work) = queue.pending.pop() {
                local.push(work);
                for _ in 1..BATCH {
                    let Some(work) = queue.pending.pop() else {
                        break;
                    };
                    local.push(work);
                }
                queue.active += 1;
                *active = true;
                return true;
            }
            if queue.active == 0 {
                queue.done = true;
                self.available.notify_all();
                return false;
            }
            queue = self.available.wait(queue).unwrap();
        }
    }
}

struct Worker {
    pending: Vec<usize>,
    labs: Vec<old::LocalAllocation>,
    used: usize,
    promoted: usize,
    #[cfg(feature = "trace-counts")]
    counts: TraceCounts,
}
impl Worker {
    fn push(&mut self, scheduler: &Scheduler, work: usize) {
        if self.pending.len() == LOCAL {
            scheduler.share(&mut self.pending, LOCAL / 2);
        }
        self.pending.push(work);
    }
}

pub(super) struct Graph<'a> {
    old: old::ParallelOld<'a>,
    _forwarded: &'a mut Forwarding,
    bits: *mut usize,
    small: *mut Option<Pointer>,
    // Per-owner states avoid contended read-modify-write operations on packed
    // bitmap words: 0 = untouched, 1 = copying, 2 = forwarding pointer published.
    // The compact serial forwarding bitmap is materialized once after joining.
    claimed: &'a [AtomicU8],
    variables: &'a [VariableObject],
    ranges: &'a [ChunkRange],
    range_index: &'a RangeIndex,
    spaces: &'a [Space],
    descriptors: &'static [Descriptor],
    major: bool,
    scheduler: Scheduler,
    workers: Vec<UnsafeCell<Worker>>,
}
// SAFETY: frozen nursery indices/spaces are read only. Forwarding bits and owner
// claims are atomic; a successful claim grants exclusive copying rights. The
// pool invokes each worker index once, so its UnsafeCell is uniquely accessed.
// No application/root/drop/weak callback runs concurrently with these workers.
unsafe impl Sync for Graph<'_> {}

impl<'a> Graph<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        old: &'a mut OldHeap,
        forwarded: &'a mut Forwarding,
        variables: &'a [VariableObject],
        ranges: &'a [ChunkRange],
        range_index: &'a RangeIndex,
        spaces: &'a [Space],
        descriptors: &'static [Descriptor],
        major: bool,
        pending: &mut Worklist,
        workers: usize,
        claimed: &'a [AtomicU8],
    ) -> Result<Self> {
        let old = old::ParallelOld::new(old)?;
        debug_assert_eq!(claimed.len(), forwarded.objects);
        let mut locals = reserved(workers)?;
        for _ in 0..workers {
            let mut labs = reserved(old.bin_count())?;
            labs.resize(old.bin_count(), old::LocalAllocation::default());
            #[cfg(feature = "trace-counts")]
            let mut counts = TraceCounts {
                kinds: reserved(descriptors.len())?,
                ..TraceCounts::default()
            };
            #[cfg(feature = "trace-counts")]
            counts.kinds.resize(descriptors.len(), [0; 2]);
            locals.push(UnsafeCell::new(Worker {
                pending: reserved(LOCAL)?,
                labs,
                used: 0,
                promoted: 0,
                #[cfg(feature = "trace-counts")]
                counts,
            }));
        }
        let pending = std::mem::replace(
            pending,
            Worklist {
                blocks: Vec::new(),
                len: 0,
                capacity: 0,
            },
        );
        Ok(Self {
            old,
            bits: forwarded.bits.as_mut_ptr(),
            small: forwarded.small.as_mut_ptr(),
            _forwarded: forwarded,
            claimed,
            variables,
            ranges,
            range_index,
            spaces,
            descriptors,
            major,
            workers: locals,
            scheduler: Scheduler {
                queue: Mutex::new(Queue {
                    pending,
                    active: 0,
                    done: false,
                }),
                available: Condvar::new(),
            },
        })
    }

    pub fn capacity_bytes(&self) -> usize {
        self.old.capacity_bytes()
            + self.workers.capacity() * size_of::<UnsafeCell<Worker>>()
            + self.workers.len()
                * (LOCAL * size_of::<usize>()
                    + self.old.bin_count() * size_of::<old::LocalAllocation>())
    }

    pub fn roots(&mut self, trace: &mut impl FnMut(&mut Tracer<'_>)) {
        let _abort = pool::AbortOnUnwind;
        // SAFETY: workers have not started. Only the caller accesses worker 0.
        let worker = unsafe { &mut *self.workers[0].get() };
        let mut tracer = Tracer {
            inner: TracerKind::Parallel(ParallelTracer {
                graph: self,
                worker,
            }),
        };
        trace(&mut tracer);
    }
    pub fn remembered(&mut self) {
        // SAFETY: worker loops have not started yet.
        let worker = unsafe { &mut *self.workers[0].get() };
        if !self.major {
            self.old
                .seed_remembered(|work| worker.push(&self.scheduler, work));
        }
        let count = worker.pending.len();
        self.scheduler.share(&mut worker.pending, count);
    }
    pub fn trace(&self, pool: &mut pool::Pool) {
        pool.run(|index| self.work(index));
        let words = self.claimed.len().div_ceil(usize::BITS as usize);
        let workers = pool.workers();
        pool.run(|index| {
            for word in (words * index / workers)..(words * (index + 1) / workers) {
                let first = word * usize::BITS as usize;
                let end = (first + usize::BITS as usize).min(self.claimed.len());
                let mut bits = 0;
                for object in first..end {
                    let state = self.claimed[object].load(Ordering::Relaxed);
                    debug_assert_ne!(state, 1, "no copying owner may remain after join");
                    bits |= usize::from(state == 2) << (object - first);
                }
                // SAFETY: graph work is joined, and this job owns this disjoint
                // bitmap word. Serial weak/drop lookup starts only after join.
                unsafe { self.bits.add(word).write(bits) };
            }
        });
    }
    fn work(&self, index: usize) {
        // SAFETY: Pool assigns this index to exactly one participating thread.
        let worker = unsafe { &mut *self.workers[index].get() };
        let mut active = false;
        while self.scheduler.take(&mut worker.pending, &mut active) {
            while let Some(work) = worker.pending.pop() {
                let (pointer, bytes, kind) = self.old.trace_info(work, self.descriptors);
                let mut tracer = Tracer {
                    inner: TracerKind::Parallel(ParallelTracer {
                        graph: self,
                        worker,
                    }),
                };
                // SAFETY: this owner was exclusively claimed/marked, copied and
                // published before enqueuing. The integration opted into worker tracing.
                unsafe { (self.descriptors[kind].trace)(pointer, bytes, &mut tracer) };
            }
        }
    }

    pub fn finish(mut self, #[cfg(feature = "trace-counts")] counts: &mut TraceCounts) {
        let mut used = 0;
        let mut promoted = 0;
        for worker in &mut self.workers {
            let worker = worker.get_mut();
            self.old.return_unused(&mut worker.labs);
            used += worker.used;
            promoted += worker.promoted;
            #[cfg(feature = "trace-counts")]
            {
                counts.visits += worker.counts.visits;
                counts.young += worker.counts.young;
                counts.forwarded += worker.counts.forwarded;
                counts.promoted += worker.counts.promoted;
                counts.old += worker.counts.old;
                counts.old_marked += worker.counts.old_marked;
                for (a, b) in counts.kinds.iter_mut().zip(&worker.counts.kinds) {
                    a[0] += b[0];
                    a[1] += b[1];
                }
            }
        }
        self.old.finish(used, promoted);
    }

    fn forwarded(&self, object: &Located) -> Pointer {
        // SAFETY: callers observed the release-published forwarding bit.
        unsafe {
            if object.small == usize::MAX {
                object.source.cast::<Pointer>().as_ptr().read_unaligned()
            } else {
                self.small
                    .add(object.small)
                    .read()
                    .expect("published small forwarding pointer")
            }
        }
    }
    /// None grants this worker exclusive ownership of promotion.
    fn claim(&self, object: &Located) -> Option<Pointer> {
        let state = &self.claimed[object.index];
        if state.load(Ordering::Acquire) == 2 {
            return Some(self.forwarded(object));
        }
        if state
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return None;
        }
        while state.load(Ordering::Acquire) != 2 {
            std::hint::spin_loop();
        }
        Some(self.forwarded(object))
    }
    fn publish(&self, object: &Located, target: Pointer) {
        // SAFETY: only the claim winner writes this retired source's forwarding
        // word/table entry; others read it only after acquire publication.
        unsafe {
            if object.small == usize::MAX {
                object
                    .source
                    .cast::<Pointer>()
                    .as_ptr()
                    .write_unaligned(target);
            } else {
                self.small.add(object.small).write(Some(target));
            }
        }
        self.claimed[object.index].store(2, Ordering::Release);
    }
}

pub(super) struct ParallelTracer<'a> {
    graph: &'a Graph<'a>,
    worker: &'a mut Worker,
}
impl ParallelTracer<'_> {
    #[inline(always)]
    pub fn relocate(&mut self, pointer: Pointer) -> Pointer {
        #[cfg(feature = "trace-counts")]
        {
            self.worker.counts.visits += 1;
        }
        if self.mark_old(pointer) {
            return pointer;
        }
        let Some(object) = find(
            self.graph.variables,
            self.graph.ranges,
            self.graph.range_index,
            pointer,
        ) else {
            return pointer;
        };
        let target = self.forward(&object, |graph, worker| {
            graph.old.allocate(
                object.kind,
                &graph.spaces[object.kind],
                object.bytes,
                graph.major,
                &mut worker.labs,
            )
        });
        // SAFETY: the interior offset was checked against the unchanged payload length.
        unsafe { Pointer::new_unchecked(target.as_ptr().add(object.offset)) }
    }

    #[inline(always)]
    pub fn relocate_fixed<T, const KIND: usize>(&mut self, pointer: NonNull<T>) -> NonNull<T> {
        debug_assert_eq!(
            self.graph.descriptors[KIND].fixed_size,
            Some(size_of::<T>())
        );
        debug_assert_eq!(self.graph.descriptors[KIND].alignment, align_of::<T>());
        #[cfg(feature = "trace-counts")]
        {
            self.worker.counts.visits += 1;
        }
        if self.mark_old(pointer.cast()) {
            return pointer;
        }
        let address = pointer.as_ptr() as usize;
        let Some(range) = self.graph.range_index.find(self.graph.ranges, address) else {
            return pointer;
        };
        let extent = size_of::<T>().max(1).next_multiple_of(align_of::<T>());
        let relative = address - range.pointer.as_ptr() as usize;
        debug_assert_eq!(range.kind, KIND);
        debug_assert_eq!(relative % extent, 0);
        let local = relative / extent;
        let object = Located {
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
        };
        self.forward(&object, |graph, worker| {
            graph
                .old
                .allocate_fixed::<T, KIND>(graph.major, &mut worker.labs)
        })
        .cast()
    }

    #[inline]
    fn mark_old(&mut self, pointer: Pointer) -> bool {
        if !self.graph.major {
            return false;
        }
        if let Some((work, fresh)) = self.graph.old.mark(pointer, self.graph.descriptors) {
            #[cfg(feature = "trace-counts")]
            {
                self.worker.counts.old += 1;
                self.worker.counts.old_marked += usize::from(fresh);
            }
            if fresh {
                self.worker.push(&self.graph.scheduler, work);
            }
            true
        } else {
            false
        }
    }

    #[inline(always)]
    fn forward(
        &mut self,
        object: &Located,
        allocate: impl FnOnce(&Graph<'_>, &mut Worker) -> (Pointer, usize),
    ) -> Pointer {
        #[cfg(feature = "trace-counts")]
        {
            self.worker.counts.young += 1;
        }
        if let Some(target) = self.graph.claim(object) {
            #[cfg(feature = "trace-counts")]
            {
                self.worker.counts.forwarded += 1;
                self.worker.counts.kinds[object.kind][1] += 1;
            }
            return target;
        }
        let (target, work) = allocate(self.graph, self.worker);
        // SAFETY: the winner exclusively owns this source and destination. No
        // callback reads the destination until it is copied and queued below.
        unsafe {
            let source = object.source.as_ptr();
            let destination = target.as_ptr();
            match object.bytes {
                8 => std::ptr::copy_nonoverlapping(source, destination, 8),
                16 => std::ptr::copy_nonoverlapping(source, destination, 16),
                24 => std::ptr::copy_nonoverlapping(source, destination, 24),
                32 => std::ptr::copy_nonoverlapping(source, destination, 32),
                48 => std::ptr::copy_nonoverlapping(source, destination, 48),
                bytes => std::ptr::copy_nonoverlapping(source, destination, bytes),
            }
        }
        self.graph.publish(object, target);
        self.worker.used += self.graph.spaces[object.kind].extent_known(object.bytes);
        self.worker.promoted += 1;
        #[cfg(feature = "trace-counts")]
        {
            self.worker.counts.promoted += 1;
            self.worker.counts.kinds[object.kind][0] += 1;
        }
        self.worker.push(&self.graph.scheduler, work);
        target
    }
}
