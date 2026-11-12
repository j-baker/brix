//! Bounded work queue. Call push/pop/join only while the mutator is parked:
//! backpressure and idle workers must not delay a collection.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::sparks::{Spark, Sparks};

struct State<T> {
    jobs: VecDeque<T>,
    closed: bool,
    stopped: bool,
    workers: usize,
    active: usize,
}

pub(crate) enum Work<'h, T> {
    Primary(T),
    Spark(Spark<'h>),
}

pub(crate) struct Queue<'h, T> {
    state: Mutex<State<T>>,
    changed: Condvar,
    capacity: usize,
    pub sparks: Arc<Sparks<'h>>,
}

impl<'h, T> Queue<'h, T> {
    pub fn new(capacity: usize, workers: usize) -> Self {
        Self {
            state: Mutex::new(State {
                jobs: VecDeque::new(),
                closed: false,
                stopped: false,
                workers,
                active: 0,
            }),
            changed: Condvar::new(),
            capacity: capacity.max(1),
            sparks: Arc::new(Sparks::new(workers)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State<T>>) -> MutexGuard<'a, State<T>> {
        self.changed.wait(state).unwrap_or_else(|p| p.into_inner())
    }

    pub fn stopped(&self) -> bool {
        self.lock().stopped
    }

    pub fn push(&self, job: T) -> bool {
        let mut state = self.lock();
        while !state.closed && !state.stopped && state.jobs.len() == self.capacity {
            state = self.wait(state);
        }
        if state.closed || state.stopped {
            return false;
        }
        state.jobs.push_back(job);
        drop(state);
        self.sparks.wake();
        true
    }

    /// Jobs first: they are certainly needed. A worker without one runs
    /// sparks, and stays available for them until the last job completes.
    pub fn pop(&self) -> Option<Work<'h, T>> {
        let mut sparks = self.sparks.lock();
        loop {
            let mut state = self.lock();
            if let Some(job) = state.jobs.pop_front() {
                state.active += 1;
                self.changed.notify_all();
                return Some(Work::Primary(job));
            }
            if (state.closed || state.stopped) && state.active == 0 {
                return None;
            }
            drop(state);
            if let Some(spark) = self.sparks.take(&mut sparks) {
                return Some(Work::Spark(spark));
            }
            sparks = self.sparks.wait(sparks);
        }
    }

    pub fn primary(&self) -> Primary<'_, 'h, T> {
        Primary(self)
    }

    pub fn close(&self) {
        let finished = {
            let mut state = self.lock();
            state.closed = true;
            state.active == 0 && state.jobs.is_empty()
        };
        self.changed.notify_all();
        if finished {
            self.sparks.cancel();
        }
        self.sparks.wake();
    }

    pub fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        // Destructors are arbitrary caller code. Run them without the lock.
        let jobs = std::mem::take(&mut state.jobs);
        let finished = state.active == 0;
        self.changed.notify_all();
        drop(state);
        if finished {
            self.sparks.cancel();
        }
        self.sparks.wake();
        drop(jobs);
    }

    pub fn worker(&self) -> Worker<'_, 'h, T> {
        Worker(self)
    }

    pub fn join(&self) {
        let mut state = self.lock();
        while state.workers != 0 {
            state = self.wait(state);
        }
    }
}

pub(crate) struct Primary<'a, 'h, T>(&'a Queue<'h, T>);

impl<T> Drop for Primary<'_, '_, T> {
    fn drop(&mut self) {
        let finished = {
            let mut state = self.0.lock();
            state.active -= 1;
            (state.closed || state.stopped) && state.active == 0
        };
        // Nothing can need background work any more: stop what is running.
        if finished {
            self.0.sparks.cancel();
        }
        self.0.sparks.wake();
    }
}

pub(crate) struct Worker<'a, 'h, T>(&'a Queue<'h, T>);

impl<T> Drop for Worker<'_, '_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.stop();
        }
        self.0.lock().workers -= 1;
        self.0.changed.notify_all();
    }
}
