//! Error type for the daemon client.

use std::collections::BTreeSet;
use std::io;

use bstr::BString;

use crate::protocol::Verbosity;

/// A structured error reported by the daemon (`STDERR_ERROR`, protocol >= 1.26).
/// Its text is bytes, as Nix's is: a message can quote a name that is not
/// UTF-8 (a derivation output's, say).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteError {
    pub level: Verbosity,
    /// The main message, as Nix would print after `error:`.
    pub message: BString,
    /// The `… while …` trace lines, in the order Nix sends and prints them
    /// (the context added last comes first).
    pub traces: Vec<BString>,
}

impl RemoteError {
    /// The error as the `nix` command prints it (`showErrorInfo`): see
    /// [`DaemonError::render`].
    pub fn render(&self, show_trace: bool) -> String {
        String::from_utf8_lossy(&self.render_bytes(show_trace)).into_owned()
    }

    /// [`RemoteError::render`] with the bytes as they are.
    pub fn render_bytes(&self, show_trace: bool) -> Vec<u8> {
        let traces: Vec<&[u8]> = self.traces.iter().map(|t| t.as_slice()).collect();
        show_error_info(self.level, &traces, &self.message, show_trace)
    }
}

/// The message alone, as Nix's `Error::msg()`: what another message
/// embeds, e.g. "cannot open connection to remote store '…': <message>"
/// (bytes that are not UTF-8 replaced).
impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("I/O error talking to the Nix daemon: {0}")]
    Io(#[from] io::Error),

    /// The connection closed in the middle of a message.
    #[error("Nix daemon disconnected unexpectedly (maybe it crashed?)")]
    Disconnected,

    /// The bytes did not follow the protocol. The message is the one Nix
    /// uses for the same violation wherever it has one.
    #[error("{0}")]
    Protocol(String),

    /// The daemon socket could not be reached (`nix::connect`).
    #[error("cannot connect to socket at '{}': {}", path.display(), sys_error_text(source))]
    Connect {
        path: std::path::PathBuf,
        source: io::Error,
    },

    /// `ssh` or the remote program could not be started.
    #[error("unable to execute '{program}': {}", sys_error_text(source))]
    Spawn { program: String, source: io::Error },

    /// The handshake, or what the daemon sends right after it, failed
    /// (`RemoteStore::initConnection`). As in Nix, only the message of the
    /// underlying error is shown.
    #[error("cannot open connection to remote store '{store}': {error}")]
    OpenConnection {
        store: String,
        error: Box<DaemonError>,
    },

    #[error("access denied by the Nix daemon (check the `allowed-users` setting in the daemon)")]
    AccessDenied,

    /// The first reply was a number, but not the daemon's magic.
    #[error("Nix daemon protocol mismatch")]
    ProtocolMismatch,

    /// The first reply was not even a number (`SerialisationError` during
    /// the handshake): what the other side sent, chomped. Typically a shell
    /// or ssh message from an `ssh-ng` remote. Bytes, which Nix shows as
    /// they are (see [`DaemonError::message_bytes`]).
    #[error("protocol mismatch, got '{0}'")]
    UnexpectedReply(BString),

    #[error("Nix daemon protocol version not supported")]
    UnsupportedMajorVersion,

    #[error("the Nix daemon version is too old (speaks protocol {0}, brix needs at least {1})")]
    DaemonTooOld(
        crate::protocol::ProtocolVersion,
        crate::protocol::ProtocolVersion,
    ),

    /// The daemon rejected the operation.
    #[error("{0}")]
    Remote(RemoteError),

    /// An earlier operation failed in a way that may have left the stream
    /// out of step, so the connection takes no further operations.
    #[error("the connection to the Nix daemon is unusable after an earlier error")]
    ConnectionUnusable,

    #[error("invalid store URI '{uri}': {reason}")]
    InvalidUri { uri: String, reason: String },

    /// A store setting or environment variable Nix would reject, with
    /// Nix's message.
    #[error("{0}")]
    Config(String),

    /// What Nix rejects with a `UsageError` (a store URI it cannot parse
    /// at all, a setting value its type refuses), with Nix's message: a
    /// command line front end adds its "Try '… --help'" hint.
    #[error("{0}")]
    Usage(String),

    /// `error` with a `… while …` context line, as Nix's `addTrace`. Shown
    /// as the bare message when embedded; [`DaemonError::render`] prints
    /// the trace.
    #[error("{error}")]
    Traced {
        trace: String,
        error: Box<DaemonError>,
    },

    #[error("invalid store path: {0}")]
    InvalidStorePath(#[from] brix_store::StorePathError),

    /// The daemon sent a hash, content address or derivation brix-store rejects.
    #[error("invalid data from the Nix daemon: {0}")]
    InvalidData(String),

    #[error("failed to start SSH connection to '{0}'")]
    SshStartFailed(String),

    #[error("invalid SSH authority: '{0}': {1}")]
    InvalidSshAuthority(String, String),

    #[error("{0}")]
    Unsupported(String),
}

impl DaemonError {
    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        DaemonError::Protocol(msg.into())
    }

    /// The error as the `nix` command prints it (`showErrorInfo`): a level
    /// prefix (`error:` for everything but a daemon error sent at another
    /// level), then, if the error carries any, its `… while …` traces
    /// before the message. Without `show_trace` at most four traces are
    /// shown, then Nix's truncation notice. Bytes that are not UTF-8 (in a
    /// daemon's message) are replaced; [`DaemonError::render_bytes`] keeps
    /// them.
    pub fn render(&self, show_trace: bool) -> String {
        String::from_utf8_lossy(&self.render_bytes(show_trace)).into_owned()
    }

    /// [`DaemonError::render`] as Nix writes it: a daemon's message and
    /// traces are bytes, written as they are.
    pub fn render_bytes(&self, show_trace: bool) -> Vec<u8> {
        let mut traces: Vec<&[u8]> = Vec::new();
        let mut cur = self;
        while let DaemonError::Traced { trace, error } = cur {
            traces.push(trace.as_bytes());
            cur = error;
        }
        match cur {
            DaemonError::Remote(e) => {
                traces.extend(e.traces.iter().map(|t| t.as_slice()));
                show_error_info(e.level, &traces, &e.message, show_trace)
            }
            other => show_error_info(
                Verbosity::Error,
                &traces,
                &other.message_bytes(),
                show_trace,
            ),
        }
    }

    /// The message alone (`Error::msg()`, what `Display` shows) as Nix
    /// has it: bytes. What the daemon or the remote program sent (a
    /// daemon's error, a reply that is no protocol at all) need not be
    /// UTF-8 and is kept as it is, also where another message embeds it
    /// ("cannot open connection to remote store '…': …"); the rest is
    /// brix's own text.
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            DaemonError::Remote(e) => e.message.to_vec(),
            DaemonError::UnexpectedReply(got) => {
                [&b"protocol mismatch, got '"[..], got, b"'"].concat()
            }
            DaemonError::OpenConnection { store, error } => {
                let mut message =
                    format!("cannot open connection to remote store '{store}': ").into_bytes();
                message.extend_from_slice(&error.message_bytes());
                message
            }
            DaemonError::Traced { error, .. } => error.message_bytes(),
            other => other.to_string().into_bytes(),
        }
    }
}

/// An OS error's text as Nix's `SysError` shows it: `strerror` alone,
/// without Rust's " (os error N)".
fn sys_error_text(e: &io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(n) => text
            .strip_suffix(&format!(" (os error {n})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

/// `showErrorInfo` (`error.cc`) for an error whose traces carry no
/// positions, as every error that crosses the daemon protocol is. Bytes,
/// as Nix's messages are.
fn show_error_info(
    level: Verbosity,
    traces: &[&[u8]],
    message: &[u8],
    show_trace: bool,
) -> Vec<u8> {
    let prefix = match level {
        Verbosity::Error => "error: ",
        Verbosity::Warn => "warning: ",
        Verbosity::Notice => "note: ",
        Verbosity::Info => "info: ",
        Verbosity::Talkative => "talk: ",
        Verbosity::Chatty => "chat: ",
        Verbosity::Debug => "debug: ",
        Verbosity::Vomit => "vomit: ",
    };
    let mut body = Vec::new();
    if !traces.is_empty() {
        // `printSkippedTracesMaybe` takes the set of seen traces by value,
        // so its clearing never reaches the caller: once seen, a trace
        // counts as a duplicate for good.
        let mut seen: BTreeSet<&[u8]> = BTreeSet::new();
        let mut skipped: Vec<&[u8]> = Vec::new();
        let mut count = 0usize;
        let mut truncate = false;
        for &trace in traces {
            if trace.is_empty() {
                continue;
            }
            if !show_trace && count > 3 {
                truncate = true;
            }
            if !truncate {
                if seen.contains(trace) {
                    skipped.push(trace);
                    continue;
                }
                seen.insert(trace);
                print_skipped(&mut body, &mut skipped);
                count += 1;
                print_trace(&mut body, trace);
            }
        }
        print_skipped(&mut body, &mut skipped);
        if truncate {
            body.extend_from_slice(
                b"\n(stack trace truncated; use '--show-trace' to show the full, detailed trace)\n",
            );
        }
        body.push(b'\n');
        body.extend_from_slice(prefix.as_bytes());
    }
    body.extend_from_slice(message);
    body.push(b'\n');
    indent(
        prefix.as_bytes(),
        " ".repeat(prefix.len()).as_bytes(),
        chomp_bytes(&body),
    )
}

/// `printTrace` for a trace without a position.
fn print_trace(out: &mut Vec<u8>, trace: &[u8]) {
    out.extend_from_slice("\n… ".as_bytes());
    out.extend_from_slice(trace);
    out.push(b'\n');
}

/// `printSkippedTracesMaybe`: a few repeated traces are printed again, a
/// longer run is summarised.
fn print_skipped(out: &mut Vec<u8>, skipped: &mut Vec<&[u8]>) {
    if skipped.len() > 5 {
        out.extend_from_slice(
            format!("\n({} duplicate frames omitted)\n", skipped.len()).as_bytes(),
        );
    } else {
        for trace in skipped.iter() {
            print_trace(out, trace);
        }
    }
    skipped.clear();
}

/// `chomp`: strip trailing spaces, tabs and line ends, of bytes.
pub(crate) fn chomp_bytes(s: &[u8]) -> &[u8] {
    let end = s
        .iter()
        .rposition(|&b| !matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        .map_or(0, |i| i + 1);
    &s[..end]
}

/// `indent` (`error.cc`): prefix every line, chomping each.
fn indent(first: &[u8], rest: &[u8], s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in s.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let prefixed = [if i == 0 { first } else { rest }, line].concat();
        out.extend_from_slice(chomp_bytes(&prefixed));
    }
    out
}

impl From<brix_store::HashError> for DaemonError {
    fn from(e: brix_store::HashError) -> Self {
        DaemonError::InvalidData(e.to_string())
    }
}

impl From<brix_store::ContentAddressError> for DaemonError {
    fn from(e: brix_store::ContentAddressError) -> Self {
        DaemonError::InvalidData(e.to_string())
    }
}

impl From<brix_store::DerivationError> for DaemonError {
    fn from(e: brix_store::DerivationError) -> Self {
        DaemonError::InvalidData(e.to_string())
    }
}

impl From<brix_store::DerivedPathError> for DaemonError {
    fn from(e: brix_store::DerivedPathError) -> Self {
        match e {
            brix_store::DerivedPathError::StorePath(e) => DaemonError::InvalidStorePath(e),
            dynamic @ brix_store::DerivedPathError::Dynamic(_) => {
                DaemonError::Unsupported(dynamic.to_string())
            }
        }
    }
}

impl From<brix_store::nar::NarError> for DaemonError {
    fn from(e: brix_store::nar::NarError) -> Self {
        match e {
            brix_store::nar::NarError::Io(io) if io.kind() == io::ErrorKind::UnexpectedEof => {
                DaemonError::Disconnected
            }
            brix_store::nar::NarError::Io(io) => DaemonError::Io(io),
            other => DaemonError::Protocol(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(traces: &[&str]) -> DaemonError {
        DaemonError::Remote(RemoteError {
            level: Verbosity::Error,
            message: "builder failed".into(),
            traces: traces.iter().map(|&t| BString::from(t)).collect(),
        })
    }

    /// A daemon's message and traces are bytes, written as they are (the
    /// message of `nix build` of a floating output whose name is not
    /// UTF-8, through a daemon: see `brix-cli/tests/output_names.rs`).
    #[test]
    fn remote_text_is_bytes() {
        let e = DaemonError::Remote(RemoteError {
            level: Verbosity::Error,
            message: BString::from(&b"name 'a\xc3' contains illegal character '\xc3'"[..]),
            traces: vec![BString::from(&b"while x\xff"[..])],
        });
        let expected = [
            "error:\n       … while x".as_bytes(),
            b"\xff\n\n       error: name 'a\xc3' contains illegal character '\xc3'",
        ]
        .concat();
        assert_eq!(e.render_bytes(false), expected);
        assert_eq!(e.render(false), String::from_utf8_lossy(&expected));
    }

    #[test]
    fn errors_without_traces_are_one_line() {
        assert_eq!(remote(&[]).render(false), "error: builder failed");
        assert_eq!(
            DaemonError::Disconnected.render(false),
            "error: Nix daemon disconnected unexpectedly (maybe it crashed?)"
        );
        let warning = RemoteError {
            level: Verbosity::Warn,
            message: "careful".into(),
            traces: vec![],
        };
        assert_eq!(warning.render(false), "warning: careful");
    }

    #[test]
    fn traces_come_first_and_lines_are_chomped() {
        // The layout of `nix store info --store
        // 'ssh-ng://localhost?base64-ssh-public-host-key=!!!'`: no trailing
        // space after "error:", an empty line between entries.
        assert_eq!(
            remote(&["while building 'x'", "while realising"]).render(false),
            "error:\n       … while building 'x'\n\n       … while realising\n\n       error: builder failed"
        );
        // The bare message is what other messages embed.
        assert_eq!(
            remote(&["while building 'x'"]).to_string(),
            "builder failed"
        );
    }

    #[test]
    fn at_most_four_traces_without_show_trace() {
        let traces = ["t1", "t2", "t3", "t4", "t5", "t6"];
        let short = remote(&traces).render(false);
        assert!(short.contains("… t4"), "{short}");
        assert!(!short.contains("… t5"), "{short}");
        assert!(short.contains(
            "\n       (stack trace truncated; use '--show-trace' to show the full, detailed trace)\n\n       error: builder failed"
        ), "{short}");
        let full = remote(&traces).render(true);
        assert!(
            full.contains("… t6") && !full.contains("truncated"),
            "{full}"
        );
    }

    #[test]
    fn repeated_traces_are_summarised() {
        let mut traces = vec!["a"];
        traces.extend(["b"; 7]);
        traces.push("c");
        let out = remote(&traces).render(true);
        assert_eq!(
            out,
            "error:\n       … a\n\n       … b\n\n       (6 duplicate frames omitted)\n\n       … c\n\n       error: builder failed"
        );
        // A short run is printed again in full.
        let out = remote(&["a", "a", "b"]).render(true);
        assert_eq!(
            out,
            "error:\n       … a\n\n       … a\n\n       … b\n\n       error: builder failed"
        );
    }

    #[test]
    fn local_traces_wrap_the_message() {
        let e = DaemonError::Traced {
            trace: "while splitting NIX_SSHOPTS '-o \"x'".into(),
            error: Box::new(DaemonError::Config("unterminated double quote".into())),
        };
        assert_eq!(e.to_string(), "unterminated double quote");
        assert_eq!(
            e.render(false),
            "error:\n       … while splitting NIX_SSHOPTS '-o \"x'\n\n       error: unterminated double quote"
        );
    }

    #[test]
    fn os_errors_print_like_sys_error() {
        let e = DaemonError::Connect {
            path: "/tmp/nonexistent-sock".into(),
            source: io::Error::from_raw_os_error(2),
        };
        // `nix store info --store unix:///tmp/nonexistent-sock`.
        assert_eq!(
            e.to_string(),
            "cannot connect to socket at '/tmp/nonexistent-sock': No such file or directory"
        );
    }

    /// What a remote program sent instead of the handshake is shown as it
    /// is, also where "cannot open connection …" embeds it, as `nix store
    /// info --store 'ssh-ng://localhost?remote-program=…'` shows it for a
    /// program that prints `hello \xff garbage`. A daemon's message
    /// embedded there is bytes too.
    #[test]
    fn embedded_messages_keep_their_bytes() {
        let e = DaemonError::OpenConnection {
            store: "ssh-ng://localhost".into(),
            error: Box::new(DaemonError::UnexpectedReply(BString::from(
                &b"hello \xff garbage"[..],
            ))),
        };
        assert_eq!(
            e.render_bytes(false),
            b"error: cannot open connection to remote store 'ssh-ng://localhost': protocol mismatch, got 'hello \xff garbage'"
        );
        let e = DaemonError::OpenConnection {
            store: "daemon".into(),
            error: Box::new(DaemonError::Remote(RemoteError {
                level: Verbosity::Error,
                message: BString::from(&b"user '\xe9' is not allowed"[..]),
                traces: vec![],
            })),
        };
        assert_eq!(
            e.message_bytes(),
            b"cannot open connection to remote store 'daemon': user '\xe9' is not allowed"
        );
    }
}
