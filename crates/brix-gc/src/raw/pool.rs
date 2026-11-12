//! A persistent, allocation-free phase runner. Jobs borrow the collector only
//! until every worker has returned; the calling thread participates as worker 0.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

/// The closure of the `run` in progress. It is borrowed from that call's
/// frame; `run` clears it before returning, so the lifetime is never observed
/// to end.
type Job = &'static (dyn Fn(usize) + Sync);

struct State {
    job: Option<Job>,
    generation: usize,
    pending: usize,
    stop: bool,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    finished: Condvar,
}
pub(super) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

/// Unwinding during a moving collection cannot leave roots and payloads in
/// different phases, or strand another worker waiting for a publication.
pub(super) struct AbortOnUnwind;
impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        if thread::panicking() {
            std::process::abort();
        }
    }
}

impl Pool {
    pub fn new(workers: usize) -> super::Result<Self> {
        let mut pool = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    job: None,
                    generation: 0,
                    pending: 0,
                    stop: false,
                }),
                changed: Condvar::new(),
                finished: Condvar::new(),
            }),
            threads: super::reserved(workers.saturating_sub(1))?,
        };
        for worker in 1..workers {
            let shared = pool.shared.clone();
            let handle = thread::Builder::new()
                .name(format!("brix-gc-{worker}"))
                .spawn(move || {
                    let mut generation = 0;
                    loop {
                        let mut state = shared.state.lock().unwrap();
                        while !state.stop && state.generation == generation {
                            state = shared.changed.wait(state).unwrap();
                        }
                        if state.stop {
                            return;
                        }
                        generation = state.generation;
                        let job = state.job.unwrap();
                        drop(state);
                        let _abort = AbortOnUnwind;
                        job(worker);
                        let mut state = shared.state.lock().unwrap();
                        state.pending -= 1;
                        if state.pending == 0 {
                            shared.finished.notify_one();
                        }
                    }
                })
                .map_err(|_| super::Error::OutOfMemory)?;
            pool.threads.push(handle);
        }
        Ok(pool)
    }

    pub fn workers(&self) -> usize {
        self.threads.len() + 1
    }

    pub fn run<F: Fn(usize) + Sync>(&mut self, f: F) {
        let _abort = AbortOnUnwind;
        let job: &(dyn Fn(usize) + Sync) = &f;
        // SAFETY: only the lifetime changes. Workers call the job only between
        // the publication below and their decrement of `pending`; this
        // function waits for `pending` to reach zero and clears the job
        // before `f` is dropped, and aborts instead of unwinding past it.
        let job: Job = unsafe { std::mem::transmute(job) };
        {
            let mut state = self.shared.state.lock().unwrap();
            debug_assert_eq!(state.pending, 0);
            state.job = Some(job);
            state.pending = self.threads.len();
            state.generation = state.generation.wrapping_add(1);
            self.shared.changed.notify_all();
        }
        f(0);
        let mut state = self.shared.state.lock().unwrap();
        while state.pending != 0 {
            state = self.shared.finished.wait(state).unwrap();
        }
        state.job = None;
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.stop = true;
            self.shared.changed.notify_all();
        }
        for thread in self.threads.drain(..) {
            thread.join().expect("GC worker must not unwind");
        }
    }
}
