//! Cold-path thunk waiting and cross-worker recursion detection.
//!
//! Wait records are stable, ordinary allocations, never objects in the moving
//! heap. They contain no heap addresses. A waiter retains its own rooted cell,
//! enters the GC-safe parked state, waits, reenters the mutator, then reloads
//! the cell from its root. Consequently neither relocation nor address reuse
//! can invalidate an OS wait.
//!
//! The registry tracks dependencies between workers, not addresses. Completion
//! of any awaited thunk owned by B wakes B's waiters; they recheck their own
//! cells. This may wake a waiter early, but needs no per-cell ID, allocation,
//! pointer-keyed hash table, or collector rekeying. With a small fixed pool,
//! at most workers-1 other workers can wait on an owner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::WorkerId;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    Waiting,
    /// The awaited thunk was published or abandoned: force it again.
    Ready,
    /// A job needs what this spark holds (see [`WaitRegistry::register`]).
    Evicted,
}

struct Event {
    signal: Mutex<Signal>,
    changed: Condvar,
}

impl Event {
    fn new() -> Self {
        Self {
            signal: Mutex::new(Signal::Waiting),
            changed: Condvar::new(),
        }
    }

    fn signal(&self, signal: Signal) {
        *lock(&self.signal) = signal;
        self.changed.notify_one();
    }

    fn wait(&self) -> Signal {
        let mut signal = lock(&self.signal);
        while *signal == Signal::Waiting {
            signal = self
                .changed
                .wait(signal)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        *signal
    }
}

struct Dependency {
    owner: WorkerId,
    event: Arc<Event>,
}

struct Inner {
    waiting: Vec<Option<Dependency>>,
    /// Workers running a background spark. Sparks evaluate in no particular
    /// order, so a cycle of waits through one says nothing about the
    /// program: the spark gives way, never the job.
    background: Vec<bool>,
}

pub(crate) struct WaitRegistry {
    inner: Mutex<Inner>,
    cancelled: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitError {
    Recursion,
    Cancelled,
    /// A spark must leave this thunk to the ordered consumer.
    Deferred,
}

/// Dropping a ticket, including during unwinding, removes its dependency edge.
/// It never carries a reference into the movable graph.
pub(crate) struct Ticket<'a> {
    registry: &'a WaitRegistry,
    worker: WorkerId,
    event: Arc<Event>,
}

impl Ticket<'_> {
    /// The caller must enter the collector's parked state before calling this,
    /// and reenter the mutator before inspecting the cell after returning.
    /// An evicted spark must stop: a job waits for a thunk it holds.
    pub fn wait(self) -> Result<(), WaitError> {
        match self.event.wait() {
            Signal::Evicted => Err(WaitError::Deferred),
            _ => Ok(()),
        }
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut inner = lock(&self.registry.inner);
        let slot = &mut inner.waiting[self.worker.index()];
        if slot
            .as_ref()
            .is_some_and(|edge| Arc::ptr_eq(&edge.event, &self.event))
        {
            *slot = None;
        }
    }
}

impl WaitRegistry {
    pub fn new(workers: usize) -> Self {
        assert!(workers > 0);
        // Also validate the upper bound before allocating.
        WorkerId::from_index(workers - 1);
        Self {
            inner: Mutex::new(Inner {
                waiting: (0..workers).map(|_| None).collect(),
                background: vec![false; workers],
            }),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Register after observing a running cell. `still_owned` rechecks/marks
    /// the cell under this lock, closing the race with completion's notify.
    /// It must not block, allocate, collect, or call back into this registry.
    /// None means the state changed: reload the rooted cell and retry forcing.
    pub fn register(
        &self,
        worker: WorkerId,
        owner: WorkerId,
        still_owned: impl FnOnce() -> bool,
    ) -> Result<Option<Ticket<'_>>, WaitError> {
        let mut inner = lock(&self.inner);
        assert!(worker.index() < inner.waiting.len());
        assert!(owner.index() < inner.waiting.len());
        assert!(
            inner.waiting[worker.index()].is_none(),
            "a worker runs only one synchronous job"
        );
        if self.is_cancelled() {
            return Err(WaitError::Cancelled);
        }
        if !still_owned() {
            return Ok(None);
        }
        // Each worker has at most one outgoing wait edge. With insertion
        // serialized, following the chain suffices to reject every cycle.
        let mut cursor = owner;
        let mut spark = None;
        let cycle = loop {
            if cursor == worker {
                break true;
            }
            match &inner.waiting[cursor.index()] {
                Some(edge) => {
                    if spark.is_none() && inner.background[cursor.index()] {
                        spark = Some(cursor);
                    }
                    cursor = edge.owner;
                }
                None => break false,
            }
        };
        if cycle {
            // A cycle of jobs alone is the program's: ordered evaluation
            // would force the same thunks in the same nesting. A spark in
            // it only got there first. If this worker is the spark, it
            // stops; otherwise a waiting spark of the cycle is evicted, and
            // abandons the thunks it holds on its way out. Every thunk a
            // background worker owns was claimed by its current spark, and
            // that flag changes under this lock.
            if inner.background[worker.index()] {
                return Err(WaitError::Deferred);
            }
            match spark {
                Some(spark) => {
                    let edge = inner.waiting[spark.index()].take().unwrap();
                    edge.event.signal(Signal::Evicted);
                }
                None => return Err(WaitError::Recursion),
            }
        }
        let event = Arc::new(Event::new());
        inner.waiting[worker.index()] = Some(Dependency {
            owner,
            event: event.clone(),
        });
        Ok(Some(Ticket {
            registry: self,
            worker,
            event,
        }))
    }

    /// Enter or leave background evaluation. The worker owns no thunk at
    /// either transition.
    pub fn set_background(&self, worker: WorkerId, background: bool) {
        lock(&self.inner).background[worker.index()] = background;
    }

    /// Only called when publication reports waiters. Clearing edges before
    /// waking prevents a completed dependency from forming a spurious cycle.
    pub fn notify(&self, owner: WorkerId) {
        let mut inner = lock(&self.inner);
        for slot in &mut inner.waiting {
            if slot.as_ref().is_some_and(|edge| edge.owner == owner) {
                slot.take().unwrap().event.signal(Signal::Ready);
            }
        }
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// The coordinator may reset only after every batch worker has joined.
    pub fn reset(&self) {
        let inner = lock(&self.inner);
        assert!(inner.waiting.iter().all(Option::is_none));
        self.cancelled.store(false, Ordering::Release);
    }

    /// Cancellation is a runtime operation, not a memoized Nix failure.
    pub fn cancel(&self) {
        let mut inner = lock(&self.inner);
        self.cancelled.store(true, Ordering::Release);
        for slot in &mut inner.waiting {
            if let Some(edge) = slot.take() {
                edge.event.signal(Signal::Ready);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel::publication::{Claim, Publication};
    use std::sync::Barrier;

    #[test]
    fn detects_self_and_cross_worker_cycles() {
        let registry = WaitRegistry::new(3);
        let a = WorkerId::from_index(0);
        let b = WorkerId::from_index(1);
        let c = WorkerId::from_index(2);
        assert!(matches!(
            registry.register(a, a, || true),
            Err(WaitError::Recursion)
        ));
        let ab = registry.register(a, b, || true).unwrap().unwrap();
        let bc = registry.register(b, c, || true).unwrap().unwrap();
        assert!(matches!(
            registry.register(c, a, || true),
            Err(WaitError::Recursion)
        ));
        drop(ab);
        let ca = registry.register(c, a, || true).unwrap().unwrap();
        drop((bc, ca));
        assert!(lock(&registry.inner).waiting.iter().all(Option::is_none));
    }

    #[test]
    fn a_spark_in_a_cycle_gives_way_and_a_job_never_sees_it() {
        let registry = WaitRegistry::new(3);
        let job = WorkerId::from_index(0);
        let spark = WorkerId::from_index(1);
        let other = WorkerId::from_index(2);
        registry.set_background(spark, true);
        registry.set_background(other, true);

        // The spark closing a cycle stops, whoever else is in it.
        let waiting = registry.register(job, spark, || true).unwrap().unwrap();
        assert!(matches!(
            registry.register(spark, job, || true),
            Err(WaitError::Deferred)
        ));
        assert!(matches!(
            registry.register(spark, spark, || true),
            Err(WaitError::Deferred)
        ));
        drop(waiting);

        // The job closing a cycle evicts a waiting spark of it and waits.
        let spark_waits = registry.register(spark, other, || true).unwrap().unwrap();
        let other_waits = registry.register(other, job, || true).unwrap().unwrap();
        let job_waits = registry.register(job, spark, || true).unwrap().unwrap();
        assert!(matches!(spark_waits.wait(), Err(WaitError::Deferred)));
        // The spark abandons its thunks on the way out, which wakes the job.
        registry.notify(spark);
        job_waits.wait().unwrap();
        drop(other_waits);

        // A job's own cycle is still the program's.
        registry.set_background(spark, false);
        let waiting = registry.register(job, spark, || true).unwrap().unwrap();
        assert!(matches!(
            registry.register(spark, job, || true),
            Err(WaitError::Recursion)
        ));
        drop(waiting);
        assert!(lock(&registry.inner).waiting.iter().all(Option::is_none));
    }

    #[test]
    fn changed_state_does_not_create_a_cycle_or_allocate_a_ticket() {

        let registry = WaitRegistry::new(1);
        let a = WorkerId::from_index(0);
        assert!(registry.register(a, a, || false).unwrap().is_none());
    }

    #[test]
    fn signal_before_sleep_is_not_lost() {
        let registry = WaitRegistry::new(2);
        let a = WorkerId::from_index(0);
        let b = WorkerId::from_index(1);
        let ticket = registry.register(a, b, || true).unwrap().unwrap();
        registry.notify(b);
        ticket.wait().unwrap();
        // Notification removes the edge even before the waiter runs again.
        let reverse = registry.register(b, a, || true).unwrap().unwrap();
        registry.cancel();
        reverse.wait().unwrap();
    }

    #[test]
    fn completion_racing_registration_and_parking() {
        let a = WorkerId::from_index(0);
        let b = WorkerId::from_index(1);
        let registry = WaitRegistry::new(2);
        let barrier = Barrier::new(2);
        // Reuse the OS threads to exercise both sides of the registration race.
        let cell = Publication::suspended();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..2000 {
                    barrier.wait();
                    if let Some(ticket) = registry.register(b, a, || cell.mark_awaited(a)).unwrap()
                    {
                        ticket.wait().unwrap();
                    }
                    barrier.wait();
                }
            });
            for _ in 0..2000 {
                assert_eq!(cell.claim(a), Claim::Acquired);
                barrier.wait();
                if cell.abandon(a) {
                    registry.notify(a);
                }
                barrier.wait();
            }
        });
    }

    #[test]
    fn cancellation_wakes_waiters_and_refuses_new_ones() {
        let registry = WaitRegistry::new(3);
        let a = WorkerId::from_index(0);
        let b = WorkerId::from_index(1);
        let c = WorkerId::from_index(2);
        let first = registry.register(a, c, || true).unwrap().unwrap();
        let second = registry.register(b, c, || true).unwrap().unwrap();
        registry.cancel();
        first.wait().unwrap();
        second.wait().unwrap();
        assert!(matches!(
            registry.register(a, c, || true),
            Err(WaitError::Cancelled)
        ));
    }
}
