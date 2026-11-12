//! Stop-the-world rendezvous for native evaluator workers.
//!
//! Fast-path polling reads one atomic flag. A stopped/blocked worker keeps its
//! roots registered but has no borrowed heap views or unrooted live pointers.
//! The collector may then relocate its objects and root slots. Reentry waits
//! for collection to finish before the worker may reload any rooted pointer.
//!
//! This module coordinates execution only. The heap layer must enforce the
//! no-view/no-unrooted-pointer rule at every call to poll, park, or collect.
//! Nothing here makes an existing single-threaded heap Send or Sync.

use std::marker::PhantomData;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::WorkerId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Inactive,
    Active,
    Parked,
}

struct Inner {
    workers: Vec<Status>,
    active: usize,
    collecting: bool,
}

pub(crate) struct World {
    requested: AtomicBool,
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// One registered OS worker. Thread affinity also prevents moving a parked
/// native continuation to a different thread by accident.
pub(crate) struct Mutator {
    world: Arc<World>,
    worker: WorkerId,
    thread: PhantomData<Rc<()>>,
}

/// Blocking I/O and thunk waits retain this guard until ready to reenter.
/// Reentry also happens during unwinding, before evaluation cleanup runs.
pub(crate) struct Parked<'m> {
    mutator: &'m mut Mutator,
}

impl World {
    pub fn new(workers: usize) -> Arc<Self> {
        assert!(workers > 0);
        WorkerId::from_index(workers - 1);
        Arc::new(Self {
            requested: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                workers: vec![Status::Inactive; workers],
                active: 0,
                collecting: false,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn wait<'a>(&self, inner: MutexGuard<'a, Inner>) -> MutexGuard<'a, Inner> {
        self.changed
            .wait(inner)
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn enter(self: &Arc<Self>, worker: WorkerId) -> Mutator {
        let mut inner = self.lock();
        assert_eq!(
            inner.workers[worker.index()],
            Status::Inactive,
            "worker already registered"
        );
        while inner.collecting {
            inner = self.wait(inner);
        }
        inner.workers[worker.index()] = Status::Active;
        inner.active += 1;
        Mutator {
            world: self.clone(),
            worker,
            thread: PhantomData,
        }
    }

    fn park(&self, worker: WorkerId, inner: &mut Inner) {
        assert_eq!(inner.workers[worker.index()], Status::Active);
        inner.workers[worker.index()] = Status::Parked;
        inner.active -= 1;
        if inner.active == 0 {
            self.changed.notify_all();
        }
    }

    fn reenter(&self, worker: WorkerId) {
        let mut inner = self.lock();
        assert_eq!(inner.workers[worker.index()], Status::Parked);
        while inner.collecting {
            inner = self.wait(inner);
        }
        inner.workers[worker.index()] = Status::Active;
        inner.active += 1;
    }
}

impl Mutator {
    pub fn worker(&self) -> WorkerId {
        self.worker
    }

    /// Call only at a heap safepoint: root all live references, drop views, and
    /// reload from roots afterwards. No lock is taken without a GC request.
    #[inline]
    pub fn poll(&mut self) {
        if self.world.requested.load(Ordering::Acquire) {
            self.poll_slow();
        }
    }

    #[cold]
    fn poll_slow(&mut self) {
        let mut inner = self.world.lock();
        if !inner.collecting {
            return;
        }
        self.world.park(self.worker, &mut inner);
        while inner.collecting {
            inner = self.world.wait(inner);
        }
        inner.workers[self.worker.index()] = Status::Active;
        inner.active += 1;
    }

    /// Leave the heap before a potentially blocking operation. The returned
    /// guard must be dropped before reading heap objects again.
    pub fn park(&mut self) -> Parked<'_> {
        self.world.park(self.worker, &mut self.world.lock());
        Parked { mutator: self }
    }

    /// Run one collection after every active mutator has reached a safepoint.
    /// Concurrent requests park rather than block while remaining active.
    /// The callback may inspect/update all registered roots but must not
    /// reenter this World, invoke user code, or leave borrowed heap views alive.
    pub fn collect<R>(&mut self, collect: impl FnOnce() -> R) -> R {
        let mut inner = self.world.lock();
        self.world.park(self.worker, &mut inner);
        while inner.collecting {
            inner = self.world.wait(inner);
        }
        inner.collecting = true;
        self.world.requested.store(true, Ordering::Release);
        while inner.active != 0 {
            inner = self.world.wait(inner);
        }
        // Keep the gate locked: no entry/exit or root-registry teardown can
        // race the callback. Restore the rendezvous even if the callback panics.
        let result = catch_unwind(AssertUnwindSafe(collect));
        inner.collecting = false;
        self.world.requested.store(false, Ordering::Release);
        inner.workers[self.worker.index()] = Status::Active;
        inner.active += 1;
        self.world.changed.notify_all();
        drop(inner);
        match result {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }
}

impl Drop for Mutator {
    fn drop(&mut self) {
        let mut inner = self.world.lock();
        assert_eq!(inner.workers[self.worker.index()], Status::Active);
        inner.workers[self.worker.index()] = Status::Inactive;
        inner.active -= 1;
        self.world.changed.notify_all();
    }
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.mutator.world.reenter(self.mutator.worker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Barrier;

    #[test]
    fn collection_waits_for_active_workers_but_not_parked_ones() {
        let world = World::new(3);
        let barrier = Barrier::new(3);
        let finished = AtomicBool::new(false);
        let accesses = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut worker = world.enter(WorkerId::from_index(1));
                barrier.wait();
                while !finished.load(Ordering::Acquire) {
                    accesses.fetch_add(1, Ordering::Relaxed);
                    worker.poll();
                }
            });
            scope.spawn(|| {
                let mut worker = world.enter(WorkerId::from_index(2));
                let _parked = worker.park();
                barrier.wait();
                while !finished.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            });
            let mut collector = world.enter(WorkerId::from_index(0));
            barrier.wait();
            for _ in 0..100 {
                collector.collect(|| {
                    let before = accesses.load(Ordering::Relaxed);
                    std::thread::yield_now();
                    assert_eq!(accesses.load(Ordering::Relaxed), before);
                });
            }
            finished.store(true, Ordering::Release);
        });
        assert_eq!(world.lock().active, 0);
    }

    #[test]
    fn simultaneous_collectors_do_not_deadlock() {
        let world = World::new(4);
        let barrier = Barrier::new(4);
        let collections = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for index in 0..4 {
                let world = &world;
                let barrier = &barrier;
                let collections = &collections;
                scope.spawn(move || {
                    let mut worker = world.enter(WorkerId::from_index(index));
                    barrier.wait();
                    // Every worker parks while requesting the collection.
                    worker.collect(|| {
                        collections.fetch_add(1, Ordering::Relaxed);
                    });
                    // Other requesters can still need this worker to poll.
                    while collections.load(Ordering::Relaxed) != 4 {
                        worker.poll();
                        std::thread::yield_now();
                    }
                });
            }
        });
        assert_eq!(collections.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn panic_restores_collection_and_parked_state() {
        let world = World::new(1);
        let mut worker = world.enter(WorkerId::from_index(0));
        assert!(catch_unwind(AssertUnwindSafe(|| worker.collect(|| panic!("injected")))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _parked = worker.park();
            panic!("injected");
        }))
        .is_err());
        assert_eq!(worker.collect(|| 42), 42);
        assert_eq!(world.lock().active, 1);
        assert!(!world.requested.load(Ordering::Acquire));
    }
}
