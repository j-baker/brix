//! Parallel heap adapter. Each mutator retains its own bump allocator and
//! temporary roots. At a rendezvous their nurseries are transferred into the
//! coordinator's RawHeap, which owns the common old generation, and one
//! collection traces all roots and remembered writes.

use super::*;
use crate::parallel::{
    publication::{Claim, State},
    wait::{WaitError, WaitRegistry},
    world::World,
    WorkerId,
};
use std::cell::{Cell as LocalCell, UnsafeCell};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Budget {
    allocated: AtomicUsize,
    threshold: AtomicUsize,
    stress: bool,
}

pub(crate) struct LocalRaw {
    heap: RawHeap,
    dirty: Vec<NonNull<u8>>,
    observed: LocalCell<usize>,
    budget: Arc<Budget>,
    stats: Stats,
}

impl Deref for LocalRaw {
    type Target = RawHeap;
    fn deref(&self) -> &RawHeap {
        &self.heap
    }
}
impl DerefMut for LocalRaw {
    fn deref_mut(&mut self) -> &mut RawHeap {
        &mut self.heap
    }
}
impl LocalRaw {
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn write_barrier(&mut self, owner: NonNull<u8>) {
        self.dirty.push(owner);
    }
    /// Called when a nursery space needs another chunk. The common budget
    /// counts the bytes workers have allocated, as the serial collector's
    /// trigger does, not the chunks they hold: every worker keeps a chunk per
    /// kind across collections, and counting those left a pool of twenty
    /// workers almost no budget (a collection per few megabytes allocated).
    /// A worker's allocations since its last refill are the bounded slack.
    pub fn collection_due(&self, pending: usize) -> bool {
        let occupied = self.heap.young_bytes();
        let previous = self.observed.replace(occupied);
        let extra = occupied
            .checked_sub(previous)
            .expect("nursery accounting resets at GC");
        let total = if extra == 0 {
            self.budget.allocated.load(Ordering::Relaxed)
        } else {
            self.budget
                .allocated
                .fetch_add(extra, Ordering::Relaxed)
                .saturating_add(extra)
        };
        self.budget.stress
            || total.saturating_add(pending) > self.budget.threshold.load(Ordering::Relaxed)
    }
}

pub(crate) struct Local<'h> {
    pub raw: LocalRaw,
    pub registry: Rc<RootRegistry<'h>>,
    pub weak_seen: WeakSeen,
    pub visited: Vec<Weak<RefCell<HashSet<Address>>>>,
}

pub(super) struct Storage<'h>(UnsafeCell<Local<'h>>);

// SAFETY: a registered worker exclusively accesses its Local while active.
// The collector may access all Locals only inside World::collect, after every
// worker has dropped views and parked. A slot is reused only after its worker
// exits, leaving no temporary roots or borrowed views. Persistent roots use
// the separately synchronized shared registry.
unsafe impl Send for Storage<'_> {}
unsafe impl Sync for Storage<'_> {}

pub(super) struct Shared<'h> {
    world: Arc<World>,
    waits: Arc<WaitRegistry>,
    locals: Mutex<Vec<Option<Arc<Storage<'h>>>>>,
    pub roots: raw_roots::shared::Registry<'h>,
    budget: Arc<Budget>,
    config: Config,
    collections: AtomicUsize,
    /// Every worker's nursery takes its chunks from this pool, which the
    /// coordinator's collections refill.
    spare: brix_gc::raw::SpareChunkPool,
}

impl<'h> Deref for Heap<'h> {
    type Target = Local<'h>;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.storage.0.get() }
    }
}
impl DerefMut for Heap<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.storage.0.get() }
    }
}

pub(super) unsafe fn new<'h>(config: Config, workers: usize) -> Result<Heap<'h>> {
    let shared = Arc::new(Shared {
        world: World::new(workers),
        waits: Arc::new(WaitRegistry::new(workers)),
        locals: Mutex::new((0..workers).map(|_| None).collect()),
        roots: unsafe { raw_roots::shared::Registry::new(&ROOT_DESCRIPTORS) },
        budget: Arc::new(Budget {
            allocated: AtomicUsize::new(0),
            threshold: AtomicUsize::new(config.threshold_bytes),
            stress: config.stress,
        }),
        config,
        collections: AtomicUsize::new(0),
        spare: brix_gc::raw::SpareChunkPool::new(&config),
    });
    attach(shared, WorkerId::from_index(0))
}

fn attach<'h>(shared: Arc<Shared<'h>>, worker: WorkerId) -> Result<Heap<'h>> {
    let mutator = shared.world.enter(worker);
    let storage = {
        let mut locals = shared.locals.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(storage) = &locals[worker.index()] {
            storage.clone()
        } else {
            let mut raw =
                RawHeap::with_spare_chunks(shared.config, &DESCRIPTORS, shared.spare.clone())?;
            unsafe { raw.enable_parallel_tracing() };
            let storage = Arc::new(Storage(UnsafeCell::new(Local {
                raw: LocalRaw {
                    heap: raw,
                    dirty: Vec::new(),
                    observed: LocalCell::new(0),
                    budget: shared.budget.clone(),
                    stats: Stats::default(),
                },
                registry: Rc::new(unsafe { RootRegistry::new(&ROOT_DESCRIPTORS) }),
                weak_seen: WeakSeen::default(),
                visited: Vec::new(),
            })));
            locals[worker.index()] = Some(storage.clone());
            storage
        }
    };
    Ok(Heap {
        storage,
        stress: shared.config.stress,
        shared,
        mutator,
        empty_attrs: RefCell::new(None),
        empty_string: None,
        empty_list: None,
        derivation_cache: RefCell::default(),
        attribute_cache: RefCell::default(),
        #[cfg(feature = "work-counters")]
        attribute_probe_reuse: RefCell::default(),
        #[cfg(feature = "work-counters")]
        probe_reuse: RefCell::default(),
        brand: PhantomData,
    })
}

/// A transferable attachment capability; no thread-local roots cross threads.
pub(crate) struct Attachment<'h> {
    shared: Arc<Shared<'h>>,
    worker: WorkerId,
}
impl<'h> Attachment<'h> {
    pub fn attach(self) -> Result<Heap<'h>> {
        attach(self.shared, self.worker)
    }
}

impl<'h> Heap<'h> {
    pub(crate) unsafe fn new_parallel(config: Config, workers: usize) -> Result<Self> {
        unsafe { new(config, workers) }
    }
    pub fn attachment(&self, index: usize) -> Attachment<'h> {
        Attachment {
            shared: self.shared.clone(),
            worker: WorkerId::from_index(index),
        }
    }
    pub fn workers(&self) -> usize {
        self.shared
            .locals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
    pub fn poll(&mut self) -> crate::Result<()> {
        self.mutator.poll();
        if self.shared.waits.is_cancelled() {
            Err(crate::error::cancelled())
        } else {
            Ok(())
        }
    }
    pub fn cancellation(&self) -> Arc<WaitRegistry> {
        self.shared.waits.clone()
    }
    pub fn worker(&self) -> WorkerId {
        self.mutator.worker()
    }
    pub(crate) fn collection_epoch(&self) -> usize {
        self.shared.collections.load(Ordering::Relaxed)
    }
    pub fn set_background(&self, background: bool) {
        self.shared.waits.set_background(self.worker(), background);
    }
    pub fn park(&mut self) -> crate::parallel::world::Parked<'_> {
        self.mutator.park()
    }

    pub(super) fn collect_generation(&mut self, major: bool) -> Result<()> {
        let shared = self.shared.clone();
        let previous = shared.collections.load(Ordering::Relaxed);
        self.mutator.collect(|| {
            // A concurrent minor collection can already have satisfied this
            // allocation request while this worker waited at the rendezvous.
            if !major && shared.collections.load(Ordering::Relaxed) != previous {
                return Ok(());
            }
            unsafe { shared.collect(major) }
        })
    }

    /// An unevaluated thunk no worker has claimed (at this instant).
    pub fn is_suspended(&self, cell: CellRoot<'_, 'h>) -> bool {
        unsafe { cell.raw.read().as_ref().publication.state() == State::Suspended }
    }

    pub fn claim(&self, cell: CellRoot<'_, 'h>) -> Claim {
        unsafe { cell.raw.read().as_ref().publication.claim(self.worker()) }
    }

    pub fn wait_on(
        &mut self,
        cell: CellRoot<'_, 'h>,
        owner: WorkerId,
    ) -> std::result::Result<(), WaitError> {
        let shared = self.shared.clone();
        let ticket = shared.waits.register(self.worker(), owner, || unsafe {
            cell.raw.read().as_ref().publication.mark_awaited(owner)
        })?;
        if let Some(ticket) = ticket {
            let _parked = self.mutator.park();
            ticket.wait()?;
        }
        Ok(())
    }

    pub fn read_suspension<'v>(&'v self, cell: CellRoot<'_, 'h>) -> ValueView<'v, 'h> {
        self.decode(unsafe { cell.raw.read().as_ref().load_owned(self.worker()) })
    }

    pub fn suspension_value<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
    ) -> ValueRoot<'r, 'h> {
        roots.push::<ValueKind>(unsafe { cell.raw.read().as_ref().load_owned(self.worker()) })
    }

    pub fn store(&mut self, cell: CellRoot<'_, 'h>, value: ValueRoot<'_, 'h>) {
        self.publish(cell, value.raw.read());
    }
    pub fn store_thunk(&mut self, cell: CellRoot<'_, 'h>, env: EnvRoot<'_, 'h>, node: GlobalNode) {
        self.publish(
            cell,
            RawValue::Thunk {
                env: env.raw.read(),
                node,
            },
        );
    }
    pub fn store_blackhole(&mut self, cell: CellRoot<'_, 'h>) {
        // The owner keeps the suspension rooted; readers see the Running tag.
        let pointer = cell.raw.read();
        let value = unsafe { pointer.as_ref() };
        assert_eq!(value.publication.state(), State::Running(self.worker()));
        unsafe { value.value.get().write(RawValue::Blackhole) };
    }
    pub(super) fn publish(&mut self, cell: CellRoot<'_, 'h>, value: RawValue) {
        let pointer = cell.raw.read();
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
        let current = unsafe { pointer.as_ref() };
        match current.publication.state() {
            State::Running(owner) => {
                assert_eq!(owner, self.worker());
                unsafe { current.value.get().write(value) };
                let awaited = if matches!(value, RawValue::Thunk { .. } | RawValue::App(_)) {
                    current.publication.abandon(owner)
                } else {
                    current.publication.finish(owner)
                };
                if awaited {
                    self.shared.waits.notify(owner);
                }
            }
            // Constructor placeholders have not escaped the constructing
            // worker. Published WHNF cells must never be overwritten.
            State::Ready | State::Suspended => unsafe {
                pointer.as_ptr().write(RawCell::new(value))
            },
        }
    }
}

impl Shared<'_> {
    /// World::collect holds the rendezvous until all tracing/repair completes.
    unsafe fn collect(&self, major: bool) -> Result<()> {
        let locals = self.locals.lock().unwrap_or_else(|p| p.into_inner());
        let mut workers: Vec<_> = locals
            .iter()
            .flatten()
            .map(|s| unsafe { &mut *s.0.get() })
            .collect();
        let (first, others) = workers.split_first_mut().expect("coordinator heap");
        let transferred = others
            .iter_mut()
            .try_for_each(|worker| unsafe { first.raw.heap.absorb_nursery(&mut worker.raw.heap) });
        if let Err(error) = transferred {
            // Earlier donors may already have transferred their chunks. Their
            // payload addresses/roots remain valid; reconcile pressure before
            // returning so a later retry cannot subtract a stale capacity.
            let mut allocated = 0usize;
            for worker in workers {
                let occupied = worker.raw.heap.young_bytes();
                worker.raw.observed.set(occupied);
                allocated = allocated.saturating_add(occupied);
            }
            self.budget.allocated.store(allocated, Ordering::Relaxed);
            return Err(error);
        }
        for pointer in first.raw.dirty.drain(..) {
            first.raw.heap.write_barrier(pointer);
        }
        for worker in others.iter_mut() {
            for pointer in worker.raw.dirty.drain(..) {
                first.raw.heap.write_barrier(pointer);
            }
        }
        // Prepare weak-identity replacement sets before any pointer mutation.
        let collects_old = major || first.raw.heap.minor_collects_old();
        let mut replacements = Vec::new();
        for worker in std::iter::once(&mut **first).chain(others.iter_mut().map(|w| &mut **w)) {
            worker.visited.retain(|set| set.strong_count() != 0);
            let visits: Vec<_> = worker
                .visited
                .iter()
                .filter_map(Weak::upgrade)
                .map(|set| {
                    let updated = HashSet::with_capacity(set.borrow().len());
                    (set, updated)
                })
                .collect();
            replacements.push((worker.weak_seen.prepare(collects_old), visits));
        }
        // Split the collector and root metadata into disjoint field borrows.
        // The callbacks must never borrow the enclosing Local containing the
        // RawHeap that collect mutably borrows.
        let raw = &mut first.raw.heap;
        let inputs: Vec<_> = std::iter::once((&first.registry, &first.weak_seen))
            .chain(others.iter().map(|w| (&w.registry, &w.weak_seen)))
            .collect();
        let result = unsafe {
            let trace = |tracer: &mut Tracer<'_>| {
                self.roots.trace_roots(tracer);
                for (registry, _) in &inputs {
                    registry.trace_roots(tracer);
                }
            };
            let update = |relocation: &brix_gc::raw::Relocation<'_>| {
                for ((_, previous), (seen, visits)) in inputs.iter().zip(&mut replacements) {
                    previous.relocate(seen, relocation);
                    for (set, updated) in visits {
                        for &address in set.borrow().iter() {
                            if let Some(p) = relocation.lookup(weak_seen::pointer(address)) {
                                updated.insert(p.addr());
                            }
                        }
                    }
                }
            };
            if major {
                raw.collect(trace, update)
            } else {
                raw.collect_minor(trace, update)
            }
        };
        let stats = raw.stats().clone();
        if result.is_ok() {
            self.collections.fetch_add(1, Ordering::Relaxed);
            for (worker, (seen, visits)) in workers.iter_mut().zip(replacements) {
                worker.weak_seen.commit(seen);
                for (set, updated) in visits {
                    *set.borrow_mut() = updated;
                }
            }
            self.budget.threshold.store(
                self.config
                    .steady_threshold_bytes
                    .unwrap_or(self.config.threshold_bytes),
                Ordering::Relaxed,
            );
        }
        let mut allocated = 0usize;
        for worker in workers {
            let occupied = worker.raw.heap.young_bytes();
            worker.raw.observed.set(occupied);
            worker.raw.stats = stats.clone();
            allocated = allocated.saturating_add(occupied);
        }
        self.budget.allocated.store(allocated, Ordering::Relaxed);
        result
    }
}
