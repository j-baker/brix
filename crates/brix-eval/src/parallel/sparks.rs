//! Background evaluation of a derivation's dependencies.
//!
//! `derivationStrict` coerces its attributes in order, and most of that time is
//! spent instantiating the derivations they refer to. Like Nix's parallel
//! evaluator, it therefore offers a walk of its arguments to the idle workers:
//! the walk forces the (typically cheap) attribute thunks and offers the
//! `outPath` of every derivation it finds as a further spark. Forcing that
//! instantiates the dependency, which offers a walk of its own arguments in
//! turn.
//!
//! A spark is an ordinary evaluation: it claims thunks and publishes their
//! values, so the ordered consumer (and every other spark) finds them done, or
//! waits for the worker that is doing them. Evaluating a spark differs from
//! evaluating a job in what it refuses to do:
//!
//! - It performs no ordered effect: no trace or warning, no fetch, no build,
//!   no embedding callback. It stops there instead ([`ErrorKind::Deferred`]).
//! - It memoizes no failure. An error restores the thunk; the consumer
//!   evaluates it at its own position and reports it with its own trace.
//! - It yields to jobs. A spark may wait for a thunk another worker is
//!   evaluating, but if a job then comes to wait for a thunk the spark
//!   holds, the spark is evicted: it stops, and the job takes over.
//!
//! Whatever a spark abandons is restored to its suspension, so nothing a spark
//! does or fails to do changes what the consumer computes or prints. The one
//! observable difference from ordered evaluation is laziness itself: a value
//! printed by `trace` or in an error message before the consumer reached it
//! may already be evaluated rather than `«thunk»`.
//!
//! Sparks are best-effort. The queue of evaluations is bounded and drops its
//! oldest entries: work the consumer has most likely caught up with.
//!
//! [`ErrorKind::Deferred`]: crate::error::ErrorKind::Deferred

use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::ErrorKind;
use crate::managed_eval::Machine;
use crate::managed_heap::{
    BindingsKind, BindingsRoot, CellKind, CellRoot, Lease, Roots, ValueView, Visited,
};
use crate::managed_language::Language;
use crate::symbol::sym;
use crate::value::SrcPos;

/// Values nested deeper than this below a derivation's arguments are left to
/// the consumer. Structured attributes can nest; nothing else is coercible.
const MAX_WALK_DEPTH: usize = 16;

/// How many times a spark gives way at a busy thunk before it waits there.
/// Each retry evaluates the spark's unfinished spine again.
const MAX_YIELDS: u8 = 3;

/// A spark that has run for less than this has little to lose by stopping.
pub(crate) const CHEAP_TO_RETRY: Duration = Duration::from_micros(100);

/// What a spark does at a thunk another worker is evaluating.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Busy {
    /// Wait for it (a spark waits for other sparks only).
    Wait,
    /// Stop, to be retried later, if the spark started (at this instant)
    /// less than [`CHEAP_TO_RETRY`] ago and other sparks are queued;
    /// otherwise wait.
    Yield(Instant),
    /// Stop: a walk only discovers work, and that thunk is in hand.
    Skip,
}

/// A list element that takes this long to evaluate makes the walk offer the
/// rest of its list as one spark per element, instead of evaluating them one
/// after another (`symlinkJoin.paths` over a workspace's test derivations,
/// say). Cheap elements, the common case, are not worth a spark each.
const EXPENSIVE_ELEMENT: Duration = Duration::from_micros(250);

pub(crate) enum Task<'h> {
    /// Walk the arguments of one `derivationStrict` call.
    Walk(Lease<'h, BindingsKind>),
    /// Evaluate a value, then walk it: one element of an expensive list, or
    /// a value a walk left at a busy thunk.
    Element(Lease<'h, CellKind>),
    /// As `Element`, for a thunk another worker was evaluating when a walk
    /// reached it: wait for its value, then walk that. Unlike the others
    /// this is not out of date once some worker has the thunk in hand.
    Await(Lease<'h, CellKind>),
    /// Force one cell: the `outPath` of a derivation found by a walk.
    Force(Lease<'h, CellKind>, SrcPos),
}

pub(crate) struct Spark<'h> {
    task: Task<'h>,
    /// The call depth of the `derivationStrict` call this work serves, so a
    /// spark reaches `max-call-depth` no later than the consumer would.
    depth: u32,
    yields: u8,
}

/// What idle workers take, evaluation before walks.
///
/// Evaluation first, because a walk mostly shadows a worker that is already
/// evaluating the same derivation's attributes, in the same order: it finds
/// work for others, but does little that the other worker would not do next.
/// With fewer idle workers than queued evaluations there is nothing to find.
#[derive(Default)]
pub(crate) struct Queued<'h> {
    /// Newest last, and taken first: the consumer reaches the oldest soonest
    /// (as did the workers before this one), so the newest is the least
    /// likely to be in hand already.
    evaluations: VecDeque<Spark<'h>>,
    /// The `derivationStrict` calls in progress that no worker has walked,
    /// by age. The oldest is taken first: it is the outermost, with the
    /// most left to find (`symlinkJoin` over a workspace was called before
    /// any of the derivations it joins). A call that returns takes its
    /// entry with it, so this holds no more than the workers' call stacks.
    walks: BTreeMap<u64, Spark<'h>>,
    next_walk: u64,
}

/// One `derivationStrict` call's offer of its arguments, withdrawn on drop.
pub(crate) struct OpenWalk<'h> {
    sparks: Arc<Sparks<'h>>,
    id: u64,
}

impl Drop for OpenWalk<'_> {
    fn drop(&mut self) {
        // The root handle is released outside the lock.
        let unwalked = self.sparks.lock().walks.remove(&self.id);
        drop(unwalked);
    }
}

pub(crate) struct Sparks<'h> {
    queue: Mutex<Queued<'h>>,
    changed: Condvar,
    capacity: usize,
    /// Set once no job can still need background work. Running sparks stop at
    /// their next safepoint; nothing further is queued.
    cancelled: Arc<AtomicBool>,
}

impl<'h> Sparks<'h> {
    pub fn new(workers: usize) -> Self {
        Self {
            queue: Mutex::new(Queued::default()),
            changed: Condvar::new(),
            capacity: workers.saturating_mul(8).max(4096),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The lock idle workers wait under. Producers of other work take it to
    /// wake them (see [`Sparks::wake`]).
    pub fn lock(&self) -> MutexGuard<'_, Queued<'h>> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn wait<'a>(&self, queue: MutexGuard<'a, Queued<'h>>) -> MutexGuard<'a, Queued<'h>> {
        self.changed.wait(queue).unwrap_or_else(|p| p.into_inner())
    }

    /// Wake idle workers to recheck the job queue. Callers must have released
    /// that queue's lock: idle workers take this lock first, then that one.
    pub fn wake(&self) {
        let _queue = self.lock();
        self.changed.notify_all();
    }

    pub fn take(&self, queue: &mut Queued<'h>) -> Option<Spark<'h>> {
        queue
            .evaluations
            .pop_back()
            .or_else(|| queue.walks.pop_first().map(|(_, walk)| walk))
    }

    /// Whether evaluation is queued that a worker could run instead of
    /// waiting at a busy thunk.
    pub fn queued(&self) -> bool {
        !self.lock().evaluations.is_empty()
    }

    /// Put a spark that gave way back where it is taken last.
    fn retry(&self, mut spark: Spark<'h>) {
        spark.yields += 1;
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        // Root handles are released outside the lock.
        let rejected = {
            let mut queue = self.lock();
            let queue = &mut queue.evaluations;
            if queue.len() < self.capacity {
                queue.push_front(spark);
                self.changed.notify_one();
                None
            } else {
                Some(spark)
            }
        };
        drop(rejected);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Root handles are released outside the lock.
        let dropped = std::mem::take(&mut *self.lock());
        drop(dropped);
    }

    fn offer(&self, spark: Spark<'h>) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let stale = {
            let mut queue = self.lock();
            let queue = &mut queue.evaluations;
            queue.push_back(spark);
            let stale = (queue.len() > self.capacity)
                .then(|| queue.pop_front())
                .flatten();
            self.changed.notify_one();
            stale
        };
        drop(stale);
    }

    /// Called by `derivationStrict` on entry, on jobs and sparks alike. The
    /// offer stands for as long as the call runs.
    pub fn offer_walk(
        self: &Arc<Self>,
        machine: &Machine<'h>,
        attrs: BindingsRoot<'_, 'h>,
        depth: u32,
    ) -> Option<OpenWalk<'h>> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let walk = Spark {
            task: Task::Walk(machine.heap.persist(attrs)),
            depth,
            yields: 0,
        };
        let mut queue = self.lock();
        let id = queue.next_walk;
        queue.next_walk += 1;
        queue.walks.insert(id, walk);
        self.changed.notify_one();
        Some(OpenWalk {
            sparks: self.clone(),
            id,
        })
    }

    /// Run one spark on an idle worker. Its outcome is only what it published.
    pub fn run(&self, language: &Language<'_>, machine: &mut Machine<'h>, spark: Spark<'h>) {
        // Many walks find the same dependency. Once any worker has it in
        // hand, the other sparks for it have nothing to add.
        if let Task::Force(cell, _) | Task::Element(cell) = &spark.task {
            if !machine.heap.is_suspended(cell.root()) {
                language.legacy().stats.sparks_stale.fetch_add(1);
                return;
            }
        }
        machine.yielded = false;
        machine.busy = match spark.task {
            Task::Walk(_) => Busy::Skip,
            _ if spark.yields < MAX_YIELDS => Busy::Yield(Instant::now()),
            _ => Busy::Wait,
        };
        let legacy = language.legacy();
        let depth = legacy.call_depth.replace(spark.depth);
        debug_assert!(machine.background.is_none());
        machine.background = Some(self.cancelled.clone());
        machine.heap.set_background(true);
        crate::runtime::set_background(true);
        legacy.stats.sparks_started.fetch_add(1);
        let result = catch_unwind(AssertUnwindSafe(|| {
            machine.with_roots(|machine, roots| match &spark.task {
                Task::Force(cell, pos) => language.force_cell(machine, roots, cell.root(), *pos),
                Task::Walk(attrs) => {
                    let walk = Walk {
                        sparks: self,
                        visited: machine.heap.visited(),
                        depth: spark.depth,
                    };
                    walk.set(language, machine, attrs.root(), 0)
                }
                Task::Element(cell) | Task::Await(cell) => {
                    // Unlike a walk, give way and retry at a busy thunk: the
                    // evaluation of this element is the point of the spark.
                    language.force_cell(machine, roots, cell.root(), SrcPos::NONE)?;
                    machine.busy = Busy::Skip;
                    let walk = Walk {
                        sparks: self,
                        visited: machine.heap.visited(),
                        depth: spark.depth,
                    };
                    walk.cell(language, machine, roots, cell.root(), 1)
                }
            })
        }));
        crate::runtime::set_background(false);
        machine.heap.set_background(false);
        machine.background = None;
        legacy.call_depth.set(depth);
        machine.busy = Busy::Wait;
        match result {
            Ok(Ok(())) => {
                legacy.stats.sparks_completed.fetch_add(1);
            }
            Ok(Err(_)) => {
                legacy.stats.sparks_deferred.fetch_add(1);
                if machine.yielded && !matches!(spark.task, Task::Walk(_)) {
                    legacy.stats.sparks_retried.fetch_add(1);
                    self.retry(spark);
                }
            }
            Err(panic) => resume_unwind(panic),
        }
    }
}

struct Walk<'s, 'h> {
    sparks: &'s Sparks<'h>,
    /// Shared values (a package set reachable from many attributes, a value
    /// containing itself) are walked once.
    visited: Visited<'h>,
    depth: u32,
}

impl<'h> Walk<'_, 'h> {
    fn set(
        &self,
        language: &Language<'_>,
        machine: &mut Machine<'h>,
        set: BindingsRoot<'_, 'h>,
        depth: usize,
    ) -> crate::Result<()> {
        let names: Vec<_> = machine
            .heap
            .bindings_view(set)
            .iter()
            .map(|attr| attr.name)
            .collect();
        for name in names {
            machine.with_roots(|machine, roots| {
                let attr = machine
                    .heap
                    .bindings_view(set)
                    .get(name)
                    .expect("an attribute set keeps its names")
                    .root(roots);
                self.cell(language, machine, roots, attr.value, depth)
            })?;
        }
        Ok(())
    }

    /// An error here ends the whole walk; a value the walk cannot evaluate
    /// (an ordered effect, a failure, a thunk a job is evaluating) is skipped.
    fn cell(
        &self,
        language: &Language<'_>,
        machine: &mut Machine<'h>,
        roots: &Roots<'_, 'h>,
        cell: CellRoot<'_, 'h>,
        depth: usize,
    ) -> crate::Result<()> {
        machine.poll_background()?;
        if depth > MAX_WALK_DEPTH || !machine.heap.mark_visited(&self.visited, cell) {
            return Ok(());
        }
        machine.skipped = false;
        if let Err(error) = language.force_cell(machine, roots, cell, SrcPos::NONE) {
            if error.kind == ErrorKind::Interrupted {
                return Err(error);
            }
            // The walk does not wait at a busy thunk, but what it left
            // unevaluated may hold dependencies: a spark of its own waits.
            // That holds for a thunk another worker has in hand, too. Its
            // owner evaluates it and no further, and may be a consumer
            // that is then held up elsewhere.
            if std::mem::take(&mut machine.skipped) {
                let cell_lease = machine.heap.persist(cell);
                self.sparks.offer(Spark {
                    task: if machine.heap.is_suspended(cell) {
                        Task::Element(cell_lease)
                    } else {
                        Task::Await(cell_lease)
                    },
                    depth: self.depth,
                    yields: 0,
                });
            }
            return machine.poll_background();
        }
        machine.with_roots(|machine, roots| match machine.heap.read(cell) {
            ValueView::Attrs(attrs) => {
                // String coercion calls `__toString`, an arbitrary function,
                // and otherwise reads `outPath` alone.
                if attrs.get(sym::TO_STRING).is_some() {
                    return Ok(());
                }
                if let Some(out_path) = attrs.get(sym::OUT_PATH) {
                    let out_path = out_path.root(roots);
                    if machine.heap.is_suspended(out_path.value) {
                        self.sparks.offer(Spark {
                            task: Task::Force(machine.heap.persist(out_path.value), out_path.pos),
                            depth: self.depth,
                            yields: 0,
                        });
                    }
                    return Ok(());
                }
                let set = attrs.root(roots);
                self.set(language, machine, set, depth + 1)
            }
            ValueView::List(list) => {
                let len = list.len();
                let value = list.root(roots);
                let mut expensive = false;
                for index in 0..len {
                    machine.with_roots(|machine, roots| {
                        let ValueView::List(list) = machine.heap.view(value) else {
                            unreachable!("a list value stays a list")
                        };
                        let element = list.get(index).unwrap().root(roots);
                        if expensive && machine.heap.is_suspended(element) {
                            self.sparks.offer(Spark {
                                task: Task::Element(machine.heap.persist(element)),
                                depth: self.depth,
                                yields: 0,
                            });
                            return Ok::<(), crate::Error>(());
                        }
                        // A collection pauses every worker: such a sample
                        // says nothing about the element.
                        let started = (Instant::now(), machine.heap.collection_epoch());
                        self.cell(language, machine, roots, element, depth + 1)?;
                        expensive |= started.1 == machine.heap.collection_epoch()
                            && started.0.elapsed() >= EXPENSIVE_ELEMENT;
                        Ok(())
                    })?;
                }
                Ok(())
            }
            _ => Ok(()),
        })
    }
}
