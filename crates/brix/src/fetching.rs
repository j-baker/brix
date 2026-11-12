//! Fetching locked trees, in the foreground and in the background.
//!
//! Evaluation is single-threaded and asks for one tree at a time: Nix's
//! `call-flake.nix` fetches an input when something forces it, and the
//! lock algorithm fetches a flake's new inputs one after the other. brix
//! keeps those semantics but starts the fetches it can foresee early, on a
//! pool of `http-connections` threads, so that they overlap (the
//! foreground's own transfers count against the same limit, which in Nix
//! is on all transfers at once):
//!
//! * locking announces every fetch a flake's inputs need before making
//!   the first (`brix_flake::LockDeps::prefetch`);
//! * evaluation starts a flake's other inputs when it first needs one of
//!   them (`brix_flake::prefetch_plan`);
//! * `flake prefetch-inputs` starts them all.
//!
//! [`Fetching`] is the part of the host that fetches: a locked tree from
//! the local store when it is there, else from its source. It is shared
//! with the pool's threads, so it holds nothing the evaluator owns.
//! [`Prefetcher`] runs it in the background and hands each result to the
//! first foreground request with the same attributes, which therefore
//! sees exactly what fetching it in the foreground would have produced,
//! error included. What a background fetch logs is held back and written
//! when the foreground takes it, so stderr says what Nix's own fetch would
//! say, where it would say it. A background fetch that was never asked for
//! is simply dropped, messages and all (its tree stays in the fetch cache
//! for the next run); only the server sees that it was made.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use brix_flake::{Attrs, FetchSettings, Fetcher, Input};
use brix_store::{StoreDir, StorePath};

use crate::error::{Error, Result};
use crate::host::Tree;

/// The fetcher a host uses: shared with the background threads.
pub(crate) type SharedFetcher = Arc<dyn Fetcher + Send + Sync>;

/// Fetches locked inputs; thread-safe (see the module documentation).
pub(crate) struct Fetching {
    pub store_dir: StoreDir,
    pub settings: FetchSettings,
    /// `None` when the session has fetching disabled.
    pub fetcher: Option<SharedFetcher>,
    pub daemon: Arc<Mutex<crate::store::DaemonState>>,
}

impl Fetching {
    /// The fetcher, or the error a session without one reports.
    pub fn fetcher(&self, input: &Input) -> Result<&(dyn Fetcher + Send + Sync)> {
        self.fetcher.as_deref().ok_or_else(|| {
            Error::Usage(
                format!(
                    "fetching is disabled in this session (while fetching the input '{input}')"
                )
                .into(),
            )
        })
    }

    /// A locked tree for `input`: from the local store when it is there,
    /// else fetched (`Input::getAccessor` with `fetch_input`'s checks). Not
    /// for `path:` inputs naming a store path, which the host serves
    /// itself.
    pub fn fetch(&self, input: &Input) -> Result<Tree> {
        if let Some(tree) = self.local_store_tree(input)? {
            return Ok(tree);
        }
        let fetched = brix_flake::fetch_input(self.fetcher(input)?, &self.settings, input)?;
        let store_path = fetched.store_path(&self.store_dir)?;
        Ok(Tree {
            accessor: fetched.accessor,
            input: fetched.input,
            store_path,
            nar_hash: None,
            from_store: false,
        })
    }

    /// Whether [`Fetching::fetch`] would most likely find the tree on this
    /// machine (the local store, the fetch cache) and fetch nothing. Asks
    /// no daemon: a store path that is present but not valid makes this
    /// wrong, which costs only a foreground fetch.
    pub fn has_locally(&self, input: &Input) -> bool {
        let Some(fetcher) = self.fetcher.as_deref() else {
            return true;
        };
        let in_store = || -> brix_flake::Result<bool> {
            Ok(match fetcher.locked_candidate(&self.settings, input)? {
                Some(c) => brix_flake::reuse_from_store(&self.store_dir, &c)?.is_some(),
                None => false,
            })
        };
        matches!(in_store(), Ok(true))
            || matches!(fetcher.has_locally(&self.settings, input), Ok(true))
    }

    /// `Input::getAccessorUnchecked`'s shortcut: an input with a `narHash`
    /// (or one the persistent cache can complete) names the store path
    /// `makeFixedOutputPath(NixArchive, narHash, name)`. When that path
    /// exists on this machine, and the daemon (if there is one) agrees it
    /// is valid, the tree is read from there instead of being fetched,
    /// with the lock checks `fetch_input` applies to a fetched tree.
    ///
    /// A `path:` input is read in place unless it has a `narHash` (a lock
    /// file's, or `fetchTree`'s, which Nix marks final for having one):
    /// Nix too takes that from the store when the store has it, even once
    /// its source is gone.
    pub fn local_store_tree(&self, input: &Input) -> Result<Option<Tree>> {
        if input.type_name().ok() == Some("path") && input.nar_hash()?.is_none() {
            return Ok(None);
        }
        // A Git commit to verify is verified by the fetcher, which Nix
        // skips only for a final input (a lock file's, or `fetchTree`'s
        // with a `narHash`) found in the store by its own `narHash`.
        if brix_flake::native::git_verify::wants_verification(input)?
            && !(input.is_final() && input.nar_hash()?.is_some())
        {
            return Ok(None);
        }
        // Without a fetcher only a `narHash` finds the tree.
        let candidate = match self.fetcher.as_deref() {
            Some(fetcher) => fetcher.locked_candidate(&self.settings, input)?,
            None => brix_flake::NoFetcher.locked_candidate(&self.settings, input)?,
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let Some(fetched) = brix_flake::reuse_from_store(&self.store_dir, &candidate)? else {
            return Ok(None);
        };
        let store_path = fetched.store_path(&self.store_dir)?;
        if self.daemon_says_invalid(&store_path)? {
            return Ok(None);
        }
        let fetched = brix_flake::fetch_input(&Ready::new(fetched), &self.settings, input)?;
        Ok(Some(Tree {
            accessor: fetched.accessor,
            input: fetched.input,
            store_path,
            nar_hash: None,
            from_store: true,
        }))
    }

    /// Whether the daemon says `path` is *not* valid in its store: a
    /// directory that exists but was never registered (or was left behind
    /// by an interrupted copy) must not be reused. Without a store (or
    /// once the session has ended) the directory is trusted the way Nix
    /// trusts its store (KNOWN_DEVIATIONS "Store directories without a
    /// store"); a store that cannot be reached is an error, as Nix's
    /// `isValidPath` would be.
    fn daemon_says_invalid(&self, path: &StorePath) -> Result<bool> {
        let mut daemon = self.daemon.lock().expect("daemon state poisoned");
        match daemon.try_client()? {
            Some(client) => Ok(!client.is_valid_path(path)?),
            None => Ok(false),
        }
    }
}

/// A fetcher that hands out one tree it was given: how a tree found in
/// the store goes through `fetch_input`'s finalisation and lock checks.
struct Ready(std::cell::Cell<Option<brix_flake::FetchedTree>>);

impl Ready {
    fn new(tree: brix_flake::FetchedTree) -> Ready {
        Ready(std::cell::Cell::new(Some(tree)))
    }
}

impl Fetcher for Ready {
    fn fetch(
        &self,
        _settings: &FetchSettings,
        input: &Input,
    ) -> brix_flake::Result<brix_flake::FetchedTree> {
        self.0.take().ok_or_else(|| {
            brix_flake::Error::Message(format!("the tree for '{input}' was already taken"))
        })
    }
}

/// One background fetch.
struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

enum SlotState {
    /// Waiting for a thread of the pool.
    Queued,
    Running,
    /// The outcome, and the messages the fetch logged, held back until
    /// the foreground takes it (see [`crate::log::hold`]).
    Finished(Result<Tree>, Vec<crate::log::Held>),
    /// The fetch panicked; the panic is raised again in the foreground
    /// request that takes it, as if that request had fetched, after the
    /// messages the fetch logged.
    Panicked(Box<dyn std::any::Any + Send>, Vec<crate::log::Held>),
    /// The foreground took the fetch over before a thread started it, or
    /// the session ended: the thread that picks it up does nothing.
    Cancelled,
}

/// The transfers allowed at once, background and foreground together
/// (`http-connections` limits all of Nix's transfers). The foreground
/// goes first: evaluation is waiting for it, and only for it.
struct Permits {
    state: Mutex<PermitState>,
    freed: Condvar,
}

struct PermitState {
    /// Transfers that may still start; `None` for no limit.
    free: Option<usize>,
    /// The foreground is waiting for a transfer: background ones wait.
    foreground_waiting: bool,
}

/// One transfer's share of [`Permits`], given back when dropped.
struct Permit<'a>(&'a Permits);

impl Permits {
    fn new(limit: Option<usize>) -> Permits {
        Permits {
            state: Mutex::new(PermitState {
                free: limit,
                foreground_waiting: false,
            }),
            freed: Condvar::new(),
        }
    }

    /// Waits until a transfer may start.
    fn acquire(&self, foreground: bool) -> Permit<'_> {
        let mut state = self.state.lock().expect("transfer permits poisoned");
        if foreground {
            state.foreground_waiting = true;
        }
        while state.free == Some(0) || (!foreground && state.foreground_waiting) {
            state = self.freed.wait(state).expect("transfer permits poisoned");
        }
        if let Some(n) = state.free.as_mut() {
            *n -= 1;
        }
        if foreground {
            state.foreground_waiting = false;
            // Background transfers held back for it may go now.
            self.freed.notify_all();
        }
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("transfer permits poisoned");
        if let Some(n) = state.free.as_mut() {
            *n += 1;
        }
        self.0.freed.notify_all();
    }
}

/// Fetches inputs in the background (see the module documentation).
pub(crate) struct Prefetcher {
    fetching: Arc<Fetching>,
    threads: usize,
    /// Shared by the pool's jobs and the foreground: at most `threads`
    /// transfers at once in all.
    permits: Arc<Permits>,
    /// Created on the first background fetch.
    pool: std::sync::OnceLock<Option<rayon::ThreadPool>>,
    /// Background fetches not yet handed to the foreground, by the
    /// attributes they were started with.
    slots: Mutex<HashMap<Attrs, Arc<Slot>>>,
}

impl Prefetcher {
    /// At most `threads` fetches at a time, in the background and in
    /// the foreground together; 0 means no limit (`http-connections`), and
    /// a pool of one thread per CPU, as for Nix's thread pools.
    pub fn new(fetching: Arc<Fetching>, threads: usize) -> Prefetcher {
        Prefetcher {
            fetching,
            threads,
            permits: Arc::new(Permits::new((threads > 0).then_some(threads))),
            pool: std::sync::OnceLock::new(),
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Starts fetching `input` in the background, unless it is already
    /// being fetched or is not worth it: only inputs that come from
    /// elsewhere (archives, tarballs, Git) and are not on this machine
    /// already are, so a run with everything cached starts no thread.
    pub fn start(&self, input: Input) {
        if !worth_prefetching(&input) {
            return;
        }
        let mut slots = self.slots.lock().expect("prefetch slots poisoned");
        if slots.contains_key(&input.attrs) || self.fetching.has_locally(&input) {
            return;
        }
        let Some(pool) = self.pool() else {
            return;
        };
        let slot = Arc::new(Slot {
            state: Mutex::new(SlotState::Queued),
            done: Condvar::new(),
        });
        slots.insert(input.attrs.clone(), slot.clone());
        let fetching = self.fetching.clone();
        let permits = self.permits.clone();
        pool.spawn_fifo(move || {
            let queued = || {
                matches!(
                    *slot.state.lock().expect("prefetch slot poisoned"),
                    SlotState::Queued
                )
            };
            if !queued() {
                return;
            }
            // The job stays queued, so that the foreground can take it
            // over, until it may transfer.
            let _permit = permits.acquire(false);
            {
                let mut state = slot.state.lock().expect("prefetch slot poisoned");
                if !matches!(*state, SlotState::Queued) {
                    return;
                }
                *state = SlotState::Running;
            }
            // A panic must not escape into the pool (rayon aborts the
            // process) nor leave the slot running for ever: it is
            // handed to the foreground like any other outcome. What the
            // fetch says waits for the foreground too: Nix, fetching the
            // input only when evaluation needs it, says it then, and not
            // at all for an input evaluation never needs.
            let (outcome, said) = crate::log::hold(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetching.fetch(&input)))
            });
            *slot.state.lock().expect("prefetch slot poisoned") = match outcome {
                Ok(result) => SlotState::Finished(result, said),
                Err(payload) => SlotState::Panicked(payload, said),
            };
            slot.done.notify_all();
        });
    }

    /// The pool, created on first use. A pool that cannot be created
    /// (the system refuses threads) means no background fetches: every
    /// fetch still happens in the foreground, when it is needed.
    fn pool(&self) -> Option<&rayon::ThreadPool> {
        self.pool
            .get_or_init(|| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(self.threads)
                    .thread_name(|i| format!("brix-fetch-{i}"))
                    .build()
                    .ok()
            })
            .as_ref()
    }

    /// Fetches `input` in the foreground: the outcome of its background
    /// fetch if one was started ([`Prefetcher::take`]), else a fetch here.
    /// A fetch here that transfers anything counts against the limit as a
    /// background one does, so that no more than `http-connections`
    /// transfers run at once in all, as in Nix: it waits for a running one
    /// to end if need be. The foreground holds no permit while it waits
    /// for a background fetch, and a job waiting for a permit is still
    /// queued (so taken over), so neither ever waits for the other.
    pub fn fetch(&self, input: &Input) -> Result<Tree> {
        if let Some(result) = self.take(&input.attrs) {
            return result;
        }
        let _permit = self.transfers(input).then(|| self.permits.acquire(true));
        self.fetching.fetch(input)
    }

    /// Whether fetching `input` transfers anything: it comes from
    /// elsewhere and is not on this machine already. What the pool fetches.
    fn transfers(&self, input: &Input) -> bool {
        worth_prefetching(input) && !self.fetching.has_locally(input)
    }

    /// The outcome of the background fetch started with exactly these
    /// attributes, waiting for it if it is still running. `None` if there
    /// is none, or if no thread has started it yet: it is then cancelled,
    /// and the caller is better off fetching it at once than queueing
    /// behind fetches nobody has asked for yet. A fetch that panicked
    /// panics here, in the caller.
    fn take(&self, attrs: &Attrs) -> Option<Result<Tree>> {
        let slot = self
            .slots
            .lock()
            .expect("prefetch slots poisoned")
            .remove(attrs)?;
        let mut state = slot.state.lock().expect("prefetch slot poisoned");
        loop {
            match std::mem::replace(&mut *state, SlotState::Cancelled) {
                SlotState::Queued | SlotState::Cancelled => return None,
                SlotState::Finished(result, said) => {
                    drop(state);
                    crate::log::release(&said);
                    return Some(result);
                }
                SlotState::Panicked(payload, said) => {
                    drop(state);
                    crate::log::release(&said);
                    std::panic::resume_unwind(payload)
                }
                SlotState::Running => {
                    *state = SlotState::Running;
                    state = slot.done.wait(state).expect("prefetch slot poisoned");
                }
            }
        }
    }

    /// Abandons the fetches no thread has started; running ones finish
    /// on their own and their results are dropped. The session calls
    /// this before it closes the store, so that no fetch starts after.
    pub fn cancel_queued(&self) {
        let slots = self.slots.lock().expect("prefetch slots poisoned");
        for slot in slots.values() {
            let mut state = slot.state.lock().expect("prefetch slot poisoned");
            if matches!(*state, SlotState::Queued) {
                *state = SlotState::Cancelled;
            }
        }
    }
}

impl Drop for Prefetcher {
    /// Queued fetches are abandoned with the session.
    fn drop(&mut self) {
        self.cancel_queued();
    }
}

/// Whether fetching `input` early can save time: it comes from elsewhere
/// and is not read in place.
fn worth_prefetching(input: &Input) -> bool {
    use brix_flake::Scheme;
    match input.scheme {
        Some(
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut | Scheme::Tarball | Scheme::File,
        ) => true,
        Some(Scheme::Git) => matches!(brix_flake::native::git::local_workdir(input), Ok(None)),
        _ => false,
    }
}

impl std::fmt::Debug for Prefetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prefetcher")
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::mpsc;
    use std::time::Duration;

    use brix_flake::Attr;

    use super::*;
    use crate::store::DaemonState;

    /// Longer than any of these fetches takes.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// A tarball input, which is worth fetching in the background.
    fn input(name: &str) -> Input {
        Input::from_attrs(
            &FetchSettings::default(),
            [
                ("type".to_owned(), Attr::String("tarball".into())),
                (
                    "url".to_owned(),
                    Attr::String(format!("https://example.org/{name}.tar.gz")),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap()
    }

    fn name_of(input: &Input) -> String {
        match input.attrs.get("url") {
            Some(Attr::String(url)) => url["https://example.org/".len()..]
                .trim_end_matches(".tar.gz")
                .to_owned(),
            other => panic!("a test input has a URL: {other:?}"),
        }
    }

    /// Fetches nothing: each fetch reports that it started, waits until
    /// the test releases it, then fails (or panics, if asked to).
    struct Gate {
        started: Mutex<mpsc::Sender<String>>,
        released: Mutex<BTreeSet<String>>,
        release: Condvar,
        panics: BTreeSet<String>,
        /// Fetches running now, and the most at once.
        in_flight: Mutex<(usize, usize)>,
    }

    impl Gate {
        fn new(panics: &[&str]) -> (Arc<Gate>, mpsc::Receiver<String>) {
            let (tx, rx) = mpsc::channel();
            let gate = Gate {
                started: Mutex::new(tx),
                released: Mutex::new(BTreeSet::new()),
                release: Condvar::new(),
                panics: panics.iter().map(|n| (*n).to_owned()).collect(),
                in_flight: Mutex::new((0, 0)),
            };
            (Arc::new(gate), rx)
        }

        fn open(&self, name: &str) {
            self.released.lock().unwrap().insert(name.to_owned());
            self.release.notify_all();
        }

        fn max_in_flight(&self) -> usize {
            self.in_flight.lock().unwrap().1
        }
    }

    impl Fetcher for Gate {
        fn fetch(
            &self,
            _settings: &FetchSettings,
            input: &Input,
        ) -> brix_flake::Result<brix_flake::FetchedTree> {
            let name = name_of(input);
            crate::log::stderr(&format!("fetching '{name}'"));
            {
                let mut n = self.in_flight.lock().unwrap();
                n.0 += 1;
                n.1 = n.1.max(n.0);
            }
            self.started.lock().unwrap().send(name.clone()).unwrap();
            let released = self.released.lock().unwrap();
            let (released, timeout) = self
                .release
                .wait_timeout_while(released, PATIENCE, |r| {
                    !r.contains(&name) && !r.contains("*")
                })
                .unwrap();
            assert!(!timeout.timed_out(), "'{name}' was never released");
            drop(released);
            self.in_flight.lock().unwrap().0 -= 1;
            if self.panics.contains(&name) {
                panic!("the fetcher panicked on '{name}'");
            }
            Err(brix_flake::Error::Message(format!("fetched '{name}'")))
        }
    }

    fn prefetcher(gate: &Arc<Gate>, threads: usize) -> (Prefetcher, Arc<Fetching>) {
        let fetching = Arc::new(Fetching {
            store_dir: StoreDir::default(),
            settings: FetchSettings::default(),
            fetcher: Some(gate.clone()),
            daemon: Arc::new(Mutex::new(DaemonState::None)),
        });
        (Prefetcher::new(fetching.clone(), threads), fetching)
    }

    fn started(rx: &mpsc::Receiver<String>) -> String {
        rx.recv_timeout(PATIENCE).expect("a fetch started")
    }

    /// The message of the error a taken fetch failed with (its first line;
    /// the trace names the input).
    fn taken(p: &Prefetcher, name: &str) -> Option<String> {
        p.take(&input(name).attrs).map(|r| {
            let e = r.map(|_| ()).unwrap_err().to_string();
            e.lines().next().unwrap_or_default().to_owned()
        })
    }

    /// A background fetch that panics panics in the foreground request
    /// that takes it; the pool lives on.
    #[test]
    fn a_panicking_fetch_panics_in_the_foreground() {
        let (gate, rx) = Gate::new(&["a"]);
        gate.open("*");
        let (p, _) = prefetcher(&gate, 1);
        p.start(input("a"));
        assert_eq!(started(&rx), "a");
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| taken(&p, "a")))
            .expect_err("the panic is raised again");
        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some("the fetcher panicked on 'a'")
        );
        p.start(input("b"));
        assert_eq!(started(&rx), "b");
        assert_eq!(taken(&p, "b").as_deref(), Some("fetched 'b'"));
    }

    /// What a background fetch logs waits for the foreground: it is written
    /// when the fetch is taken, on the taking thread, in the order it was
    /// logged, before the outcome (a panic included) is handed over.
    #[test]
    fn a_background_fetch_speaks_when_taken() {
        let (gate, rx) = Gate::new(&["b"]);
        gate.open("*");
        let (p, _) = prefetcher(&gate, 2);
        p.start(input("a"));
        p.start(input("b"));
        let mut names = vec![started(&rx), started(&rx)];
        names.sort();
        assert_eq!(names, ["a", "b"]);
        let (outcome, said) = crate::log::hold(|| taken(&p, "a"));
        assert_eq!(outcome.as_deref(), Some("fetched 'a'"));
        assert_eq!(said, [crate::log::Held::Message(b"fetching 'a'".to_vec())]);
        let (panicked, said) = crate::log::hold(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| taken(&p, "b")))
        });
        assert!(panicked.is_err(), "the panic is raised again");
        assert_eq!(said, [crate::log::Held::Message(b"fetching 'b'".to_vec())]);
    }

    /// A request for a fetch no thread has started takes it over: the
    /// pool skips it.
    #[test]
    fn a_queued_fetch_is_taken_over() {
        let (gate, rx) = Gate::new(&[]);
        let (p, _) = prefetcher(&gate, 1);
        p.start(input("a"));
        assert_eq!(started(&rx), "a");
        p.start(input("b"));
        assert_eq!(taken(&p, "b"), None, "queued: the caller fetches it");
        p.start(input("c"));
        gate.open("*");
        // The pool runs its jobs in order: `c` starting means `b`'s turn
        // has passed.
        assert_eq!(started(&rx), "c");
        assert_eq!(taken(&p, "a").as_deref(), Some("fetched 'a'"));
        assert_eq!(taken(&p, "c").as_deref(), Some("fetched 'c'"));
        assert!(rx.try_recv().is_err(), "'b' was never fetched");
    }

    /// `http-connections` threads fetch at once, and no more.
    #[test]
    fn the_pool_runs_http_connections_fetches_at_once() {
        for threads in [1, 2] {
            let (gate, rx) = Gate::new(&[]);
            let (p, _) = prefetcher(&gate, threads);
            for name in ["a", "b", "c"] {
                p.start(input(name));
            }
            // The first ones start together, in either order.
            let mut first: Vec<String> = (0..threads).map(|_| started(&rx)).collect();
            first.sort();
            assert_eq!(first, ["a", "b"][..threads]);
            assert!(
                rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "{threads} thread(s): another fetch started"
            );
            gate.open("*");
            let rest: Vec<String> = (threads..3).map(|_| started(&rx)).collect();
            assert_eq!(rest, ["a", "b", "c"][threads..]);
            for name in ["a", "b", "c"] {
                assert_eq!(taken(&p, name), Some(format!("fetched '{name}'")));
            }
            assert_eq!(gate.max_in_flight(), threads);
        }
    }

    /// A fetch in the foreground counts against `http-connections` too:
    /// with one connection it waits for the background fetch running, and
    /// a fetch it takes over from the queue is made once, here.
    #[test]
    fn the_foreground_shares_http_connections() {
        let (gate, rx) = Gate::new(&[]);
        let (p, _) = prefetcher(&gate, 1);
        p.start(input("a"));
        assert_eq!(started(&rx), "a");
        p.start(input("b"));
        std::thread::scope(|s| {
            let foreground = s.spawn(|| {
                let c = p.fetch(&input("c")).map(|_| ()).unwrap_err().to_string();
                let b = p.fetch(&input("b")).map(|_| ()).unwrap_err().to_string();
                (c, b)
            });
            assert!(
                rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "a second transfer started"
            );
            gate.open("a");
            assert_eq!(started(&rx), "c");
            gate.open("c");
            // `b` was queued behind `a`: the foreground takes it over and
            // fetches it itself, once.
            assert_eq!(started(&rx), "b");
            gate.open("b");
            let (c, b) = foreground.join().unwrap();
            assert!(c.starts_with("fetched 'c'"), "{c}");
            assert!(b.starts_with("fetched 'b'"), "{b}");
        });
        assert_eq!(taken(&p, "a").as_deref(), Some("fetched 'a'"));
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "'b' was fetched twice"
        );
        assert_eq!(gate.max_in_flight(), 1);
    }

    /// Dropping the prefetcher (with its session) abandons queued fetches;
    /// the running one finishes on its own.
    #[test]
    fn dropping_abandons_queued_fetches() {
        let (gate, rx) = Gate::new(&[]);
        let (p, fetching) = prefetcher(&gate, 1);
        p.start(input("a"));
        assert_eq!(started(&rx), "a");
        p.start(input("b"));
        drop(p);
        gate.open("*");
        // Every job holds the fetching state until it is done.
        let deadline = std::time::Instant::now() + PATIENCE;
        while Arc::strong_count(&fetching) > 1 {
            assert!(std::time::Instant::now() < deadline, "the jobs never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rx.try_recv().is_err(), "'b' was never fetched");
    }

    /// After the session closed its store, a fetch still running finds no
    /// store rather than panicking on it.
    #[test]
    fn a_closed_store_is_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = StoreDir::new(dir.path().to_str().unwrap());
        // A locked input whose tree is in the store directory.
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("hello"), "hi\n").unwrap();
        let accessor = brix_store::FsAccessor::new(tree.to_str().unwrap());
        let nar_hash = brix_flake::fetch::tree_nar_hash(&accessor).unwrap();
        let mut locked = input("a");
        locked
            .attrs
            .insert("narHash".into(), Attr::String(nar_hash.to_sri()));
        let fetched = brix_flake::reuse_from_store(&store_dir, &locked).unwrap();
        assert!(fetched.is_none(), "not in the store yet");
        let store_path = brix_flake::fetch::compute_store_path(&store_dir, &locked).unwrap();
        std::fs::rename(&tree, store_dir.display(&store_path)).unwrap();

        let config = crate::Config {
            daemon: Some("unix:///nonexistent/brix-fetching-test".into()),
            ..crate::Config::default()
        };
        let daemon = Arc::new(Mutex::new(DaemonState::new(&config, &store_dir).unwrap()));
        let fetching = Fetching {
            store_dir: store_dir.clone(),
            settings: FetchSettings::default(),
            fetcher: None,
            daemon: daemon.clone(),
        };
        // An unreachable store is an error, not an invalid path.
        let err = match fetching.local_store_tree(&locked) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unreachable store is an error"),
        };
        assert!(
            err.starts_with(
                "cannot connect to socket at '/nonexistent/brix-fetching-test': No such file or directory"
            ),
            "{err}"
        );
        daemon.lock().unwrap().close();
        let tree = fetching.local_store_tree(&locked).unwrap();
        assert_eq!(tree.map(|t| t.store_path), Some(store_path));
    }
}
