//! Progress reports from long-running fetches, for a front end to show:
//! Nix's `Activity` and `Logger::startActivity`/`result(resProgress)`,
//! reduced to what fetching produces.
//!
//! The fetchers start an [`Activity`] for each input they fetch from its
//! source and for each HTTP transfer, and report the bytes a transfer has
//! received. A cache or store hit starts none: there is nothing to wait
//! for. What becomes of the reports is the [`Progress`] implementation's
//! business; brix's command line draws Nix's one-line progress bar
//! (`brix::log`), and [`NoProgress`] drops them.
//!
//! Nothing here does I/O, so the module is available without the
//! `native` feature.

use std::fmt;

/// Identifies one activity between [`Progress::start`] and
/// [`Progress::stop`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivityId(pub u64);

/// What an activity is, which decides how a front end totals it (Nix's
/// `ActivityType`, restricted to the kinds fetching produces).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    /// Fetching one input from its source; front ends count these.
    Fetch,
    /// One HTTP transfer (`actFileTransfer`); its progress is in bytes.
    FileTransfer,
    /// Anything else worth naming while it runs.
    Other,
}

/// Receives activity reports. Every method may be called from any
/// thread: inputs are fetched in parallel.
pub trait Progress: Send + Sync + fmt::Debug {
    /// An activity begins. `text` is what Nix's progress bar would show
    /// for it (`downloading 'https://…'`).
    fn start(&self, kind: ActivityKind, text: &str) -> ActivityId;

    /// `done` units out of `expected` (0 when unknown); for a
    /// [`ActivityKind::FileTransfer`] the units are bytes.
    fn update(&self, id: ActivityId, done: u64, expected: u64);

    /// The activity is over, whether it succeeded or not.
    fn stop(&self, id: ActivityId);

    /// A thread starts waiting for activities to finish. A front end that
    /// draws on the terminal may limit drawing to such waits, when the
    /// waiting thread writes nothing else (see [`Waiting`]).
    fn begin_wait(&self) {}

    /// The wait started by the matching [`Progress::begin_wait`] is over.
    fn end_wait(&self) {}
}

/// Discards every report.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn start(&self, _kind: ActivityKind, _text: &str) -> ActivityId {
        ActivityId(0)
    }

    fn update(&self, _id: ActivityId, _done: u64, _expected: u64) {}

    fn stop(&self, _id: ActivityId) {}
}

/// A running activity: started by [`Activity::new`], stopped when
/// dropped, so an early return or an error ends it too.
#[must_use = "the activity stops when this is dropped"]
pub struct Activity<'p> {
    progress: &'p dyn Progress,
    id: ActivityId,
}

impl<'p> Activity<'p> {
    pub fn new(progress: &'p dyn Progress, kind: ActivityKind, text: &str) -> Activity<'p> {
        Activity {
            progress,
            id: progress.start(kind, text),
        }
    }

    /// See [`Progress::update`].
    pub fn update(&self, done: u64, expected: u64) {
        self.progress.update(self.id, done, expected);
    }
}

impl Drop for Activity<'_> {
    fn drop(&mut self) {
        self.progress.stop(self.id);
    }
}

/// A wait for activities, from [`Progress::begin_wait`] until dropped.
#[must_use = "the wait ends when this is dropped"]
pub struct Waiting<'p> {
    progress: &'p dyn Progress,
}

impl<'p> Waiting<'p> {
    pub fn new(progress: &'p dyn Progress) -> Waiting<'p> {
        progress.begin_wait();
        Waiting { progress }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.progress.end_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Progress for Recorder {
        fn start(&self, kind: ActivityKind, text: &str) -> ActivityId {
            let mut log = self.0.lock().unwrap();
            log.push(format!("start {kind:?} {text}"));
            ActivityId(log.len() as u64)
        }
        fn update(&self, id: ActivityId, done: u64, expected: u64) {
            self.0
                .lock()
                .unwrap()
                .push(format!("update {} {done}/{expected}", id.0));
        }
        fn stop(&self, id: ActivityId) {
            self.0.lock().unwrap().push(format!("stop {}", id.0));
        }
        fn begin_wait(&self) {
            self.0.lock().unwrap().push("wait".into());
        }
        fn end_wait(&self) {
            self.0.lock().unwrap().push("done waiting".into());
        }
    }

    #[test]
    fn guards_stop_what_they_started() {
        let r = Recorder::default();
        {
            let _wait = Waiting::new(&r);
            let act = Activity::new(&r, ActivityKind::FileTransfer, "downloading 'x'");
            act.update(1, 2);
        }
        assert_eq!(
            *r.0.lock().unwrap(),
            [
                "wait",
                "start FileTransfer downloading 'x'",
                "update 2 1/2",
                "stop 2",
                "done waiting"
            ]
        );
    }
}
