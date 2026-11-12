//! Error types.
//!
//! Nix reports most flake problems as a plain `Error` with a formatted
//! message, and URL problems as `BadURL`. brix keeps the same message texts
//! (they are observable in `nix` output and tests compare them) and only
//! adds enough structure for callers to tell the classes apart.
//!
//! Like Nix's `ErrorInfo`, an error is a message plus "while …" traces
//! (`addTrace`). `Display` shows the message only (Nix's `msg()`);
//! [`Error::render`] shows everything the way `nix` prints it, and
//! [`Error::traces`] gives the traces to callers that print them their own
//! way (the evaluator, for errors that surface inside it).
//!
//! `readFlake`'s traces name a position in `flake.nix`
//! ([`Error::trace_at`]), under which Nix shows the code there. This
//! crate has no code to show, so the host that evaluated the file renders
//! those errors itself, from [`Error::traces_at`].

use std::fmt;

use crate::flake::FlakePos;

/// An error from flakeref parsing, lock file handling or fetching.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `BadURL` in Nix: a URL or flakeref that does not parse.
    #[error("{0}")]
    BadUrl(String),
    /// A general error with Nix's message text.
    #[error("{0}")]
    Message(String),
    /// A problem with a JSON document (lock file, registry, API response).
    #[error("{0}")]
    Json(String),
    /// A tree whose NAR hash is not the one asked for: Nix's
    /// `Error((unsigned int) 102, …)`, after which the process exits with
    /// 102 ([`Error::exit_status`]).
    #[error("{0}")]
    HashMismatch(String),
    /// An exception that is not a Nix `Error`: nlohmann's, for a JSON
    /// document Nix parses with `json::parse` (a Git input's
    /// `publicKeys`). Nix catches only `Error`s to add traces, so none is
    /// ever added to it ([`Error::trace`] leaves it as it is), and
    /// `handleExceptions` prints just `error: <message>`.
    #[error("{0}")]
    Foreign(String),
    /// An I/O error, with what was being done.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// The error with a "while …" trace added by a caller, like Nix's
    /// `addTrace`. Displays as the inner error's message.
    #[error("{inner}")]
    Traced {
        trace: String,
        /// The position in `flake.nix` the trace is about, for
        /// `readFlake`'s traces (`e.addTrace(state.positions[pos], …)`).
        pos: Option<FlakePos>,
        #[source]
        inner: Box<Error>,
    },
}

impl Error {
    /// Wrap an error with a trace line, as Nix's `e.addTrace({}, …)` does.
    /// A [`Error::Foreign`] error gets none.
    pub fn trace(self, trace: impl fmt::Display) -> Error {
        if self.is_foreign() {
            return self;
        }
        Error::Traced {
            trace: trace.to_string(),
            pos: None,
            inner: Box::new(self),
        }
    }

    /// Wrap an error with a trace line about the attribute of `flake.nix`
    /// at `pos`, as Nix's `e.addTrace(state.positions[pos], …)` does.
    pub fn trace_at(self, pos: &FlakePos, trace: impl fmt::Display) -> Error {
        if self.is_foreign() {
            return self;
        }
        Error::Traced {
            trace: trace.to_string(),
            pos: Some(pos.clone()),
            inner: Box::new(self),
        }
    }

    /// The message, without traces.
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// Whether this is an [`Error::Foreign`] error: one Nix shows as
    /// `error: <message>` alone, with no trace, also when it surfaces
    /// during evaluation (brix-eval's `ErrorKind::Foreign`).
    pub fn is_foreign(&self) -> bool {
        matches!(self, Error::Foreign(_))
    }

    /// The exit status Nix ends with after this error (`ErrorInfo::status`):
    /// 102 for a hash mismatch, else 1.
    pub fn exit_status(&self) -> u32 {
        let mut cur = self;
        while let Error::Traced { inner, .. } = cur {
            cur = inner;
        }
        match cur {
            Error::HashMismatch(_) => 102,
            _ => 1,
        }
    }

    /// The traces, outermost (added last) first: the order Nix prints
    /// them in.
    pub fn traces(&self) -> Vec<&str> {
        self.traces_at()
            .into_iter()
            .map(|(_, trace)| trace)
            .collect()
    }

    /// [`Error::traces`] with the position each names, if it names one
    /// ([`Error::trace_at`]).
    pub fn traces_at(&self) -> Vec<(Option<&FlakePos>, &str)> {
        let mut traces = Vec::new();
        let mut cur = self;
        while let Error::Traced { trace, pos, inner } = cur {
            traces.push((pos.as_ref(), trace.as_str()));
            cur = inner;
        }
        traces
    }

    /// The error as `nix` prints it on stderr (`showErrorInfo`, without
    /// colours): each trace as `… while …`, outermost first, then
    /// `error: <message>`. A trace's position is not shown: that needs the
    /// code of `flake.nix`, so the host that evaluated it renders such an
    /// error itself (see the module documentation).
    pub fn render(&self) -> String {
        let message = indent_message(&self.to_string());
        let traces = self.traces();
        if traces.is_empty() {
            return format!("error: {message}");
        }
        let mut out = String::from("error:\n");
        for trace in traces {
            out.push_str(&format!("       … {trace}\n\n"));
        }
        out.push_str(&format!("       error: {message}"));
        out
    }
}

/// A message as `showErrorInfo` lays it out after `error: `: chomped of
/// trailing whitespace, every line after the first indented by seven
/// spaces (the width of `error: `), and each line chomped. Colour codes
/// are filtered out only after that, so a line ending in one keeps its
/// whitespace (an argument that ends in a newline leaves a line of
/// indentation).
fn indent_message(message: &str) -> String {
    // `chomp`.
    let chomp = |s: &str| s.trim_end_matches([' ', '\n', '\r', '\t']).len();
    let message = &message[..chomp(message)];
    let mut out = String::with_capacity(message.len());
    for (i, line) in message.split('\n').enumerate() {
        if i == 0 {
            out.push_str(&line[..chomp(line)]);
        } else {
            let indented = format!("\n       {line}");
            out.push_str(&indented[..chomp(&indented).max(1)]);
        }
    }
    out
}

/// `Error::Message` from a formatted string.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::Error::Message(format!($($arg)*)) };
}

/// `Error::BadUrl` from a formatted string.
#[macro_export]
macro_rules! bad_url {
    ($($arg:tt)*) => { $crate::Error::BadUrl(format!($($arg)*)) };
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<brix_store::url::BadUrl> for Error {
    fn from(e: brix_store::url::BadUrl) -> Error {
        Error::BadUrl(e.0)
    }
}

#[cfg(test)]
mod tests {
    /// nlohmann's exceptions are not Nix `Error`s: no trace is added.
    #[test]
    fn foreign_errors_take_no_traces() {
        let e = super::Error::Foreign("[json.exception.parse_error.101] …".into())
            .trace("while fetching the input 'x'")
            .trace("while updating the flake input 'a'");
        assert!(e.is_foreign());
        assert!(e.traces().is_empty());
        assert_eq!(e.render(), "error: [json.exception.parse_error.101] …");
    }

    #[test]
    fn hash_mismatches_end_with_102_through_their_traces() {
        let e = super::Error::HashMismatch("NAR hash mismatch".into())
            .trace("while fetching the input 'x'");
        assert_eq!(e.exit_status(), 102);
        assert_eq!(err!("boom").trace("while …").exit_status(), 1);
    }

    #[test]
    fn traces_print_outermost_first() {
        let e = err!("boom")
            .trace("while fetching the input 'x'")
            .trace("while updating the flake input 'a'");
        assert_eq!(e.to_string(), "boom");
        assert_eq!(e.message(), "boom");
        assert_eq!(
            e.traces(),
            [
                "while updating the flake input 'a'",
                "while fetching the input 'x'"
            ]
        );
        assert_eq!(
            e.render(),
            "error:\n       … while updating the flake input 'a'\n\n       … while fetching the input 'x'\n\n       error: boom"
        );
        assert_eq!(err!("plain").render(), "error: plain");
    }

    /// A message of several lines is indented under `error: ` like Nix's,
    /// each line chomped, except where a colour code ends it (as git's
    /// output does in a failed commit verification).
    #[test]
    fn messages_of_several_lines_are_indented() {
        let e = err!("failed: \x1b[35;1mGood\nNo principal matched.\n\x1b[0m")
            .trace("while fetching the input 'x'");
        assert_eq!(
            e.render(),
            "error:\n       … while fetching the input 'x'\n\n       error: failed: \x1b[35;1mGood\n       No principal matched.\n       \x1b[0m"
        );
        assert_eq!(err!("a  \n\n  b\n\n").render(), "error: a\n\n         b");
    }

    /// A trace's position is kept for the host to show, outermost first
    /// like the traces; `traces` and `render` leave it out.
    #[test]
    fn traces_keep_their_positions() {
        let pos = |shown: &str, handle| crate::flake::FlakePos {
            shown: shown.into(),
            handle,
        };
        let e = err!("boom")
            .trace_at(
                &pos("/f/flake.nix:2:3", 7),
                "while evaluating flake attribute 'url'",
            )
            .trace("while updating the flake input 'a'");
        assert_eq!(
            e.traces_at(),
            [
                (None, "while updating the flake input 'a'"),
                (
                    Some(&pos("/f/flake.nix:2:3", 7)),
                    "while evaluating flake attribute 'url'"
                ),
            ]
        );
        assert_eq!(
            e.traces(),
            [
                "while updating the flake input 'a'",
                "while evaluating flake attribute 'url'"
            ]
        );
        assert_eq!(
            e.render(),
            "error:\n       … while updating the flake input 'a'\n\n       … while evaluating flake attribute 'url'\n\n       error: boom"
        );
    }
}
