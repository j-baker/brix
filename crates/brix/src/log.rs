//! Messages for stderr, written the way Nix's logger writes them.
//!
//! [`stderr`] applies `filterANSIEscapes` to each message as a whole, so
//! colour codes disappear unless stderr is a terminal Nix would colour
//! ([`is_tty`]) and tabs expand to eight-column stops counted across the
//! entire message. The line goes to stderr, or to the [`Logger`] the
//! program installed (Nix's global `logger`): brix's command line
//! installs its progress bar, which writes messages above itself. A
//! thread can hold its messages back for a while ([`hold`]).

use std::cell::RefCell;
use std::io::Write;
use std::sync::{Arc, OnceLock};

pub use brix_eval::{filter_ansi_escapes, filter_ansi_escapes_str, indent_message};

/// Where logged lines go instead of straight to stderr.
pub trait Logger: Send + Sync {
    /// Writes one logged line: filtered for the terminal and ended by a
    /// newline.
    fn write_line(&self, line: &[u8]);
}

static LOGGER: OnceLock<Arc<dyn Logger>> = OnceLock::new();

/// Installs the process's logger, which [`stderr`] hands every line to
/// from now on. There is one per process, as there is one stderr; a
/// second one is refused and handed back.
pub fn set_logger(logger: Arc<dyn Logger>) -> Result<(), Arc<dyn Logger>> {
    LOGGER.set(logger)
}

/// Writes one logged message (a `trace:` line, a warning, a whole
/// rendered error, a build's output) to stderr, through the installed
/// [`Logger`] if there is one.
pub fn stderr(message: &str) {
    stderr_bytes(message.as_bytes());
}

/// [`stderr`] for a message that quotes Nix values (an evaluation error,
/// a `trace`), which need not be UTF-8: its bytes are written as they
/// are.
pub fn stderr_bytes(message: &[u8]) {
    let held = HELD.with(|held| match held.borrow_mut().as_mut() {
        Some(entries) => {
            entries.push(Held::Message(message.to_vec()));
            true
        }
        None => false,
    });
    if held {
        return;
    }
    let text = log_line(message, is_tty());
    match LOGGER.get() {
        Some(logger) => logger.write_line(&text),
        None => write_stderr(&text),
    }
}

/// What a thread held back while [`hold`] ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// A message logged through [`stderr`] or [`stderr_bytes`].
    Message(Vec<u8>),
    /// Bytes for stderr as they are: what a program a fetcher ran (`git`,
    /// `hg`, `ssh`) wrote to its stderr, or what brix-flake writes in a
    /// program's place (`brix_flake::native::stderr`).
    Raw(Vec<u8>),
}

thread_local! {
    /// What this thread holds back while [`hold`] runs, in order.
    static HELD: RefCell<Option<Vec<Held>>> = const { RefCell::new(None) };
}

/// Where `brix_flake::native::stderr` sends a held thread's bytes.
fn hold_raw(bytes: &[u8]) {
    HELD.with(|held| match held.borrow_mut().as_mut() {
        Some(entries) => entries.push(Held::Raw(bytes.to_vec())),
        // `hold` redirects only while it holds.
        None => write_stderr(bytes),
    });
}

/// Runs `f` with what it writes to stderr on this thread held back rather
/// than written, and returns that with its result, for [`release`] to
/// write later: the messages it logs ([`stderr`], [`stderr_bytes`]) and
/// the stderr of the programs a fetcher runs (`git`, `hg`, `ssh`, through
/// `brix_flake::native::stderr`), in order. A fetch made ahead of time on
/// a thread of the pool (`crate::fetching`) runs this way, so that what it
/// says ("unpacking '…' into the Git cache...", a retried download's
/// warnings, `git fetch`'s progress) is written when evaluation takes its
/// result, where Nix, which fetches it only then, writes it, and not at
/// all if evaluation never does. Not for nesting: an inner `hold` ends the
/// outer one's.
pub(crate) fn hold<R>(f: impl FnOnce() -> R) -> (R, Vec<Held>) {
    /// Ends the holding back when `hold` does, even by a panic.
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            HELD.with(|held| drop(held.borrow_mut().take()));
        }
    }
    HELD.with(|held| *held.borrow_mut() = Some(Vec::new()));
    let guard = Release;
    let result = brix_flake::native::stderr::redirect(hold_raw, f);
    let entries = HELD
        .with(|held| held.borrow_mut().take())
        .expect("invariant: the messages are held back until `hold` ends");
    drop(guard);
    (result, entries)
}

/// Writes what [`hold`] held back, in the order it was written.
pub(crate) fn release(entries: &[Held]) {
    for entry in entries {
        match entry {
            Held::Message(message) => stderr_bytes(message),
            Held::Raw(bytes) => brix_flake::native::stderr::write(bytes),
        }
    }
}

/// A message as Nix's loggers write it: `filterANSIEscapes(s, !isTTY)`
/// and a newline.
fn log_line(message: &[u8], tty: bool) -> Vec<u8> {
    let mut line = filter_ansi_escapes(message, !tty, usize::MAX);
    line.push(b'\n');
    line
}

/// Nix's `isTTY()`: stderr is a terminal, `TERM` is set and not `dumb`,
/// and neither `NO_COLOR` nor `NOCOLOR` is set. It decides whether what
/// `nix` logs keeps its colours and whether it draws its progress bar.
/// Computed once, as in Nix.
pub fn is_tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| {
        std::io::IsTerminal::is_terminal(&std::io::stderr())
            && std::env::var_os("TERM").is_some_and(|t| t != "dumb")
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var_os("NOCOLOR").is_none()
    })
}

/// Nix's `writeToStderr`: the bytes, as they are, on stderr.
pub fn write_stderr(text: &[u8]) {
    // A log line that cannot be written is lost, as in Nix.
    let _ = std::io::stderr().lock().write_all(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `filterANSIEscapes(s, !isTTY)`: colours stay on a terminal only.
    #[test]
    fn log_lines_keep_colours_on_a_terminal_only() {
        let message = "\u{1b}[2mhello> \u{1b}[0ma\tb";
        assert_eq!(log_line(message.as_bytes(), false), b"hello> a        b\n");
        assert_eq!(
            log_line(message.as_bytes(), true),
            "\u{1b}[2mhello> \u{1b}[0ma        b\n".as_bytes()
        );
        // Bytes that are not UTF-8 are written as they are.
        assert_eq!(log_line(b"trace: \xff", false), b"trace: \xff\n");
    }

    /// What a thread logs while it holds its messages back is returned,
    /// in order, and nothing more; afterwards it logs as before. Another
    /// thread's messages are not held.
    #[test]
    fn held_messages_are_returned_in_order() {
        let (value, held) = hold(|| {
            stderr("one");
            brix_flake::native::stderr::write(b"raw\r\xff");
            stderr_bytes(b"two \xff");
            std::thread::spawn(|| HELD.with(|held| held.borrow().is_none()))
                .join()
                .unwrap()
        });
        assert!(value, "another thread holds nothing back");
        assert_eq!(
            held,
            [
                Held::Message(b"one".to_vec()),
                Held::Raw(b"raw\r\xff".to_vec()),
                Held::Message(b"two \xff".to_vec())
            ]
        );
        assert!(HELD.with(|held| held.borrow().is_none()));
        // Holding back ends with a panic too.
        let panicked = std::panic::catch_unwind(|| hold(|| panic!("in hold")));
        assert!(panicked.is_err());
        assert!(HELD.with(|held| held.borrow().is_none()));
        // Held messages released on a thread that holds its own back join
        // those.
        let ((), again) = hold(|| release(&held));
        assert_eq!(again, held);
    }
}
