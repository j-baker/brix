//! The facade's error type.

use bstr::BString;

/// Everything that can go wrong in a [`crate::Session`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Evaluation failed. `message` is Nix's full rendering (`error: …`
    /// with traces), ready to print on stderr: bytes, as it quotes values
    /// (shown as text, ill-formed UTF-8 replaced).
    #[error("{}", String::from_utf8_lossy(.message))]
    Eval {
        message: Vec<u8>,
        /// The exit status Nix would use (1, or 102 for hash mismatches).
        exit_status: i32,
        /// A `UsageError`: the front end adds its "Try --help" hint.
        usage: bool,
    },
    /// A flake reference, lock file or fetch problem.
    #[error("{0}")]
    Flake(#[from] brix_flake::Error),
    /// The Nix daemon refused or failed.
    #[error("{0}")]
    Daemon(#[from] brix_daemon::DaemonError),
    /// A store path or derivation problem. Bytes, as it can quote a name
    /// or a derivation's environment, which need not be UTF-8 (`Display`
    /// replaces what is not).
    #[error("{0}")]
    Store(BString),
    /// The session's configuration is unusable.
    #[error("{0}")]
    Config(String),
    /// A request that cannot be served (an attribute path that does not
    /// exist, an unlocked reference in pure mode, …). Bytes, as it can
    /// quote a value or an attribute name.
    #[error("{0}")]
    Usage(BString),
    /// An operating-system error, shown as Nix's `SysError` shows it: the
    /// context, then `strerror` (without Rust's ` (os error N)`). The
    /// context is bytes, as it can name a file whose name is not UTF-8
    /// (`Display` replaces what is not).
    #[error("{context}: {}", sys_error_text(source))]
    Io {
        context: BString,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn eval(e: &brix_eval::Error, show_trace: bool) -> Error {
        Error::Eval {
            message: e.render(show_trace).into(),
            exit_status: e.exit_status.get() as i32,
            usage: e.kind == brix_eval::ErrorKind::Usage,
        }
    }

    /// This error raised inside the evaluator at `pos`. A flake error's
    /// traces become the evaluation error's, below the ones evaluation
    /// adds as the error propagates.
    pub(crate) fn into_eval(
        self,
        ev: &brix_eval::Evaluator<'_>,
        kind: brix_eval::ErrorKind,
        pos: brix_eval::value::SrcPos,
    ) -> brix_eval::Error {
        let Error::Flake(e) = self else {
            return ev.error_at(kind, self.message_bytes(), pos);
        };
        if e.is_foreign() {
            // nlohmann's exception: printed alone, never traced.
            return ev.error(brix_eval::ErrorKind::Foreign, e.message());
        }
        let err = ev.error_at(kind, e.message(), pos);
        // The evaluator keeps traces innermost first.
        for trace in e.traces().into_iter().rev() {
            err.add_trace(None, trace);
        }
        err.exit_status.set(e.exit_status());
        err
    }

    /// An I/O error under `context`, which is bytes when it names a file
    /// (`opening file "…"`), as Nix's are.
    pub fn io(context: impl Into<BString>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// The message alone, without the `error: ` prefix or traces (what
    /// `Display` shows), as bytes: the messages that quote names or values
    /// keep them as they are.
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            Error::Eval { message, .. } => message.clone(),
            Error::Store(message) | Error::Usage(message) => message.to_vec(),
            Error::Daemon(e) => e.message_bytes(),
            Error::Io { context, source } => {
                [context.as_slice(), b": ", sys_error_text(source).as_bytes()].concat()
            }
            other => other.to_string().into_bytes(),
        }
    }

    /// The process exit status Nix would use for this error.
    pub fn exit_status(&self) -> i32 {
        match self {
            Error::Eval { exit_status, .. } => *exit_status,
            // 102 for a hash mismatch.
            Error::Flake(e) => e.exit_status() as i32,
            _ => 1,
        }
    }

    /// Renders the error the way `nix` prints it on stderr. Evaluation
    /// errors already carry their `error: ` prefix (and were rendered with
    /// the session's `show-trace`); the rest get one. `show_trace` shows
    /// every trace of a daemon error, not only the first four. The bytes
    /// are what Nix writes (an evaluation error quotes values, which need
    /// not be UTF-8); [`Error::render`] is the same as text.
    pub fn render_bytes(&self, show_trace: bool) -> Vec<u8> {
        match self {
            Error::Eval { message, .. } => message.clone(),
            Error::Store(_) | Error::Usage(_) | Error::Io { .. } => {
                [&b"error: "[..], &self.message_bytes()].concat()
            }
            // A daemon's message is bytes too (it can quote an output name,
            // or a build log line).
            Error::Daemon(e) => e.render_bytes(show_trace),
            other => other.render(show_trace).into_bytes(),
        }
    }

    /// [`Error::render_bytes`] as text, ill-formed UTF-8 replaced.
    pub fn render(&self, show_trace: bool) -> String {
        match self {
            Error::Eval { message, .. } => String::from_utf8_lossy(message).into_owned(),
            Error::Flake(e) => e.render(),
            // Daemon errors carry their own level and `… while …` traces.
            Error::Daemon(e) => e.render(show_trace),
            other => format!("error: {other}"),
        }
    }
}

/// `From` for brix-store's errors: each becomes [`Error::Store`] with its
/// message, which is Nix's for the same failure (with the bytes of the
/// names it quotes, for the errors that have them).
macro_rules! store_errors {
    ($($error:ty),* $(,)?) => {$(
        impl From<$error> for Error {
            fn from(e: $error) -> Error {
                Error::Store(e.to_string().into())
            }
        }
    )*};
    (bytes: $($error:ty),* $(,)?) => {$(
        impl From<$error> for Error {
            fn from(e: $error) -> Error {
                Error::Store(e.message_bytes().into())
            }
        }
    )*};
}

store_errors!(
    brix_store::DerivedPathError,
    brix_store::MissingExperimentalFeature,
);

// Path names are bytes, and so are the messages that quote them.
store_errors!(
    bytes: brix_store::AccessorError,
    brix_store::CanonPathError,
    brix_store::ContentAddressError,
    brix_store::DerivationError,
    brix_store::HashError,
    brix_store::nar::NarError,
    brix_store::StorePathError,
    brix_store::VirtualStoreError,
);

/// A setting value Nix rejects is its `UsageError`.
impl From<brix_store::SettingError> for Error {
    fn from(e: brix_store::SettingError) -> Error {
        Error::Usage(e.to_string().into())
    }
}

/// An I/O error's text as Nix's `SysError` shows it: `strerror` alone for
/// an OS error, without the ` (os error N)` Rust appends.
pub fn sys_error_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(n) => text
            .strip_suffix(&format!(" (os error {n})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    /// `--show-trace` reaches a daemon error's rendering: all of its
    /// traces, where without it Nix shows four and a truncation notice.
    #[test]
    fn daemon_errors_render_with_show_trace() {
        let mut e = brix_daemon::DaemonError::Config("boom".into());
        for i in 0..6 {
            e = brix_daemon::DaemonError::Traced {
                trace: format!("while doing step {i}"),
                error: Box::new(e),
            };
        }
        let e = Error::Daemon(e);
        let short = e.render(false);
        let full = e.render(true);
        assert!(!short.contains("step 1"), "{short}");
        assert!(short.contains("--show-trace"), "{short}");
        assert!(full.contains("step 1") && full.contains("step 5"), "{full}");
        assert!(!full.contains("--show-trace"), "{full}");
        assert!(full.ends_with("error: boom"), "{full}");
    }

    /// Messages that name files keep the names' bytes: an I/O error's
    /// context, an accessor's error, a daemon's text embedded in another
    /// message.
    #[test]
    fn messages_keep_the_bytes_of_names() {
        let e = Error::io(
            &b"opening file \"/d\xe9/x\""[..],
            std::io::Error::from_raw_os_error(2),
        );
        assert_eq!(
            e.render_bytes(false),
            b"error: opening file \"/d\xe9/x\": No such file or directory"
        );
        let e = Error::from(brix_store::AccessorError::NotFound(b"/d\xe9".into()));
        assert_eq!(
            e.render_bytes(false),
            b"error: path '/d\xe9' does not exist"
        );
        assert_eq!(e.message_bytes(), b"path '/d\xe9' does not exist");
        let e = Error::Daemon(brix_daemon::DaemonError::OpenConnection {
            store: "ssh-ng://localhost".into(),
            error: Box::new(brix_daemon::DaemonError::UnexpectedReply(b"hi \xff".into())),
        });
        assert_eq!(
            e.message_bytes(),
            b"cannot open connection to remote store 'ssh-ng://localhost': protocol mismatch, got 'hi \xff'"
        );
    }
}
