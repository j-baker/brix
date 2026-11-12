//! What reaches stderr from below the logger: the programs fetchers run
//! (`git`, `hg`, `ssh`), whose stderr Nix leaves them (`runProgram`), and
//! the messages brix writes in the place of Nix's forked child ("executing
//! …" when a program cannot be started).
//!
//! It goes to brix's stderr, unless the thread has redirected it
//! ([`redirect`]): a fetch made ahead of time on a thread of its own holds
//! back everything it would print until evaluation needs its result (brix's
//! `log::hold`), these bytes included. A program's stderr is then piped
//! rather than inherited and passed on, bytes as they are (progress lines'
//! carriage returns included), once the program has exited: nothing else
//! is printed on the thread meanwhile, so the order is kept.

use std::cell::Cell;
use std::io::Write;
use std::process::Stdio;

/// Where a thread's stderr goes instead (see [`redirect`]).
pub type Sink = fn(&[u8]);

thread_local! {
    /// Where this thread's stderr goes instead, while [`redirect`] runs.
    static SINK: Cell<Option<Sink>> = const { Cell::new(None) };
}

/// Runs `f` with what this thread writes through this module going to
/// `sink` instead of stderr. Not for nesting: an inner `redirect` ends the
/// outer one's.
pub fn redirect<R>(sink: Sink, f: impl FnOnce() -> R) -> R {
    /// Ends the redirection when `redirect` does, even by a panic.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            SINK.set(None);
        }
    }
    SINK.set(Some(sink));
    let _restore = Restore;
    f()
}

/// Writes `bytes` to stderr as they are, or to the thread's sink.
pub fn write(bytes: &[u8]) {
    match SINK.get() {
        Some(sink) => sink(bytes),
        None => {
            // What cannot be written is lost, as with Nix's `writeToStderr`.
            let _ = std::io::stderr().lock().write_all(bytes);
        }
    }
}

/// The stderr to give a program a fetcher runs: brix's own, or a pipe when
/// the thread's stderr is redirected, whose contents [`pass_on`] takes
/// from the program's output.
pub(crate) fn for_program() -> Stdio {
    match SINK.get() {
        Some(_) => Stdio::piped(),
        None => Stdio::inherit(),
    }
}

/// Passes on what a program given [`for_program`]'s stderr wrote to it
/// (nothing when it inherited brix's).
pub(crate) fn pass_on(captured: &[u8]) {
    if !captured.is_empty() {
        write(captured);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static SEEN: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn collect(bytes: &[u8]) {
        SEEN.with(|seen| seen.borrow_mut().extend_from_slice(bytes));
    }

    /// While redirected, a program's stderr is piped and passed on with
    /// what is written directly, in order and byte for byte; afterwards
    /// programs inherit stderr again.
    #[test]
    fn a_redirected_programs_stderr_is_passed_on_in_order() {
        redirect(collect, || {
            write(b"before\n");
            let out = std::process::Command::new("sh")
                .args(["-c", "printf 'a\\rb\\n\\377' >&2"])
                .stderr(for_program())
                .output()
                .expect("sh runs");
            pass_on(&out.stderr);
            write(b"after\n");
        });
        assert_eq!(
            SEEN.with(|seen| seen.borrow().clone()),
            b"before\na\rb\n\xffafter\n"
        );
        assert!(SINK.get().is_none());
        let out = std::process::Command::new("sh")
            .args(["-c", "true"])
            .stderr(for_program())
            .output()
            .expect("sh runs");
        assert!(out.stderr.is_empty(), "inherited, not captured");
        // A panic ends the redirection too.
        let panicked = std::panic::catch_unwind(|| redirect(collect, || panic!("in redirect")));
        assert!(panicked.is_err());
        assert!(SINK.get().is_none());
    }
}
