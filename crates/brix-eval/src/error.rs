//! Evaluation errors and their rendering.
//!
//! An [`EvalError`] carries what Nix's `ErrorInfo` carries: a kind (which
//! `tryEval` and the exit status care about), a message, an optional
//! position, a list of traces added while the error propagated outwards,
//! and name suggestions. [`EvalError::render`] produces the text Nix's
//! `showErrorInfo` prints with `--show-trace`, so the language corpus can
//! be compared byte for byte.
//!
//! Messages and traces are bytes, as Nix's are: they quote values, names
//! and lines of code, which need not be UTF-8, and Nix writes them as they
//! are. `msg!` builds one from text and bytes.
//!
//! Errors are shared ([`Error`] is an `Rc`): a thunk whose evaluation
//! failed remembers its error, and forcing it again rethrows the *same*
//! object, so traces added later accumulate on it, as in Nix.

use crate::runtime::cell::{Cell, OnceCell, RefCell};
use crate::runtime::rc::Rc;
use std::collections::BTreeSet;

use brix_syntax::error::{chomp, indent};
use brix_syntax::source;
use bstr::BString;

use crate::ir::Origin;

/// The error type evaluation returns.
pub type Error = Rc<EvalError>;

/// Detach propagation context from a memoized failure. Serial evaluation keeps
/// its existing reference-counted behavior; concurrent consumers each append
/// traces to their own snapshot, never to another worker's failure.
#[inline]
pub(crate) fn snapshot(error: &Error) -> Error {
    #[cfg(not(brix_parallel))]
    {
        error.clone()
    }
    #[cfg(brix_parallel)]
    {
        let copy = EvalError::new(error.kind, error.msg.clone());
        if let Some(path) = error.invalid_path.get() {
            let _ = copy.invalid_path.set(path.clone());
        }
        *copy.pos.borrow_mut() = error.pos();
        *copy.traces.borrow_mut() = error.traces();
        *copy.suggestions.borrow_mut() = error.suggestions.borrow().clone();
        copy.exit_status.set(error.exit_status.get());
        copy
    }
}

#[cfg(brix_parallel)]
pub(crate) fn cancelled() -> Error {
    EvalError::new(ErrorKind::Interrupted, "evaluation interrupted")
}

#[cfg(brix_parallel)]
pub(crate) fn deferred() -> Error {
    EvalError::new(ErrorKind::Deferred, "retry on the ordered consumer")
}

/// The bytes of a message made of parts that are text (`&str`, `String`)
/// or bytes (`&[u8]`, `Vec<u8>`, a printed value, a symbol's name):
/// `msg!("attribute '", ev.symbol_ident(name), "' missing")`.
macro_rules! msg {
    ($($part:expr),+ $(,)?) => {{
        let mut out: ::std::vec::Vec<u8> = ::std::vec::Vec::new();
        $( out.extend_from_slice(::std::convert::AsRef::<[u8]>::as_ref(&$part)); )+
        out
    }};
}
pub(crate) use msg;

#[cfg(not(brix_parallel))]
type SourceLoader = dyn Fn() -> Option<Vec<u8>>;
#[cfg(brix_parallel)]
type SourceLoader = dyn Fn() -> Option<Vec<u8>> + Send + Sync;

/// The source text and origin of one loaded file, shared by every
/// position that points into it.
pub struct SourceInfo {
    pub origin: Origin,
    /// Set for the evaluator's own helper expressions, whose positions
    /// Nix hides from some traces (`isDerivationInternalPos`).
    pub(crate) is_internal: bool,
    /// The text, once known. A file served from a compiled image is read
    /// only when an excerpt is printed (see [`SourceInfo::lazy`]).
    source: OnceCell<Vec<u8>>,
    loader: Option<Box<SourceLoader>>,
    /// The file's length in bytes, known up front either way.
    len: u32,
    line_starts: OnceCell<Vec<u32>>,
    /// Where a lazy file's line table comes from without its text (a
    /// compiled image keeps one per module).
    line_loader: Option<Box<crate::runtime::Loader<u32>>>,
}

impl std::fmt::Debug for SourceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceInfo")
            .field("origin", &self.origin)
            .field("is_internal", &self.is_internal)
            .field("len", &self.len)
            .field("loaded", &self.source.get().is_some())
            .finish()
    }
}

impl SourceInfo {
    pub fn new(origin: Origin, source: Vec<u8>) -> SourceInfo {
        let len = u32::try_from(source.len()).expect("source file too large");
        SourceInfo {
            origin,
            is_internal: false,
            source: OnceCell::from(source),
            loader: None,
            len,
            line_starts: OnceCell::new(),
            line_loader: None,
        }
    }

    /// A file whose line table comes from `lines` and whose text is
    /// fetched by `loader`, each only when a position or an excerpt
    /// needs it. `None` from `loader` means the text is unavailable: the
    /// position still resolves, the excerpt is left out (as Nix does for
    /// a file it cannot read). `None` from `lines` (a damaged table)
    /// computes the table from the text instead.
    pub fn lazy(
        origin: Origin,
        len: u32,
        lines: Box<crate::runtime::Loader<u32>>,
        loader: Box<SourceLoader>,
    ) -> SourceInfo {
        SourceInfo {
            origin,
            is_internal: false,
            source: OnceCell::new(),
            loader: Some(loader),
            len,
            line_starts: OnceCell::new(),
            line_loader: Some(lines),
        }
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The text, fetching it on first use for a lazy file.
    pub fn source(&self) -> Option<&[u8]> {
        if let Some(s) = self.source.get() {
            return Some(s);
        }
        let loaded = (self.loader.as_ref()?)()?;
        Some(self.source.get_or_init(|| loaded))
    }

    /// The text positions and excerpts refer to (`Pos::getSource`):
    /// standard input and command-line expressions end at their first
    /// NUL, as Nix keeps them as C strings.
    fn code_text(&self) -> Option<&[u8]> {
        let source = self.source()?;
        Some(match self.origin {
            Origin::Stdin | Origin::String => source::cut_at_nul(source),
            Origin::File { .. } | Origin::Internal { .. } => source,
        })
    }

    fn line_starts(&self) -> &[u32] {
        self.line_starts.get_or_init(|| {
            self.line_loader
                .as_ref()
                .and_then(|lines| lines())
                // Nix computes the table of a text it cannot read from
                // the empty text (`getSource().value_or("")`).
                .unwrap_or_else(|| source::line_starts(self.code_text().unwrap_or_default()))
        })
    }

    /// 1-based line and column (in bytes) of a byte offset
    /// (`PosTable::operator[]`).
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        source::line_col(self.line_starts(), offset)
    }

    /// The text of a 1-based line without its terminator, if it exists
    /// and the text is available (`Pos::LinesIterator`).
    pub fn line_text(&self, line: u32) -> Option<&[u8]> {
        source::line_text(self.code_text()?, self.line_starts(), line)
    }
}

/// A resolved source position.
#[derive(Clone, Debug)]
pub struct Position {
    pub info: Rc<SourceInfo>,
    pub line: u32,
    pub column: u32,
}

impl Position {
    /// `origin:line:column`, as `Pos::print` shows it: bytes, as the
    /// file's name need not be UTF-8.
    pub fn display(&self) -> BString {
        let mut s = self.info.origin.display();
        s.push(b':');
        s.extend_from_slice(self.line.to_string().as_bytes());
        if self.column > 0 {
            s.push(b':');
            s.extend_from_slice(self.column.to_string().as_bytes());
        }
        s
    }
}

/// The class of an error. Mirrors Nix's exception hierarchy as far as it
/// is observable: `tryEval` catches `Assertion` and `Thrown` only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    /// Internal speculative fallback. Never memoized, caught by tryEval or
    /// returned to an embedding caller.
    #[cfg(brix_parallel)]
    #[doc(hidden)]
    Deferred,
    /// Cooperative cancellation of a parallel runtime. Never memoized in a thunk.
    #[cfg(brix_parallel)]
    Interrupted,
    /// Generic `EvalError`.
    Eval,
    /// `assert` failure (`AssertionError`).
    Assertion,
    /// `builtins.throw` (`ThrownError`, a kind of assertion error).
    Thrown,
    /// `builtins.abort`.
    Abort,
    TypeError,
    UndefinedVar,
    MissingArgument,
    InfiniteRecursion,
    StackOverflow,
    /// A file could not be read or does not exist, or access was denied.
    FileSystem,
    /// Restricted-mode path access.
    RestrictedPath,
    /// JSON/TOML parse errors and similar.
    Parse,
    /// Import-from-derivation or a fetch the host does not support.
    Unsupported,
    /// A `UsageError`: the command-line front end adds a "Try --help"
    /// hint after it.
    Usage,
    /// An allocation Nix could not satisfy either (`std::bad_alloc`):
    /// reported as a bare `error: out of memory`, without traces.
    OutOfMemory,
    /// An exception that is not a Nix `Error`: nlohmann's, when it is
    /// asked to write a string that is not UTF-8 as JSON. `handleExceptions`
    /// prints only `error: <message>`: no trace is ever added to it, and
    /// neither `tryEval` nor `addErrorContext` sees it.
    Foreign,
}

/// How a trace is compared for duplicate elision: its position (origin,
/// line, column) and its text.
type TraceKey = (Option<(BString, u32, u32)>, BString);

/// One "… while …" line added while the error propagated.
#[derive(Clone, Debug)]
pub struct Trace {
    pub pos: Option<Position>,
    pub hint: BString,
    /// Printed even when traces are being truncated
    /// (`TracePrint::Always`, used by `addErrorContext`).
    pub always: bool,
}

#[derive(Debug)]
pub struct EvalError {
    pub kind: ErrorKind,
    pub msg: BString,
    /// `InvalidPathError::path`: callers such as `findFile` catch this
    /// specific failure without rewriting unrelated store/build errors.
    pub(crate) invalid_path: OnceCell<brix_store::StorePath>,
    pos: RefCell<Option<Position>>,
    /// Innermost first; rendered outermost first like Nix's deque.
    traces: RefCell<Vec<Trace>>,
    suggestions: RefCell<Vec<BString>>,
    pub exit_status: Cell<u32>,
}

impl EvalError {
    pub fn new(kind: ErrorKind, msg: impl Into<BString>) -> Error {
        Rc::new(EvalError {
            kind,
            msg: msg.into(),
            invalid_path: OnceCell::new(),
            pos: RefCell::new(None),
            traces: RefCell::new(Vec::new()),
            suggestions: RefCell::new(Vec::new()),
            exit_status: Cell::new(1),
        })
    }

    /// Sets the position if none is set yet (`atPos`).
    pub fn at(self: Rc<Self>, pos: Option<Position>) -> Rc<Self> {
        if self.pos.borrow().is_none() {
            *self.pos.borrow_mut() = pos;
        }
        self
    }

    pub fn pos(&self) -> Option<Position> {
        self.pos.borrow().clone()
    }

    pub fn set_pos(&self, pos: Option<Position>) {
        *self.pos.borrow_mut() = pos;
    }

    pub fn with_trace(self: Rc<Self>, pos: Option<Position>, hint: impl Into<BString>) -> Rc<Self> {
        self.add_trace(pos, hint);
        self
    }

    pub fn add_trace(&self, pos: Option<Position>, hint: impl Into<BString>) {
        self.traces.borrow_mut().push(Trace {
            pos,
            hint: hint.into(),
            always: false,
        });
    }

    pub fn add_trace_always(&self, hint: impl Into<BString>) {
        self.traces.borrow_mut().push(Trace {
            pos: None,
            hint: hint.into(),
            always: true,
        });
    }

    pub fn traces(&self) -> Vec<Trace> {
        self.traces.borrow().clone()
    }

    pub fn set_suggestions(&self, suggestions: Vec<BString>) {
        *self.suggestions.borrow_mut() = suggestions;
    }

    /// Whether `builtins.tryEval` catches this error.
    pub fn is_catchable(&self) -> bool {
        matches!(self.kind, ErrorKind::Assertion | ErrorKind::Thrown)
    }

    /// Renders like Nix's `showErrorInfo` (without colours), including
    /// the `error:` prefix and all traces when `show_trace` is set. The
    /// result is the bytes Nix writes: the message, traces and lines of
    /// code as they are, UTF-8 or not.
    pub fn render(&self, show_trace: bool) -> BString {
        if self.kind == ErrorKind::OutOfMemory {
            // `handleExceptions` catches `std::bad_alloc` before any error
            // rendering happens.
            return "error: out of memory".into();
        }
        if self.kind == ErrorKind::Foreign {
            return msg!("error: ", self.msg).into();
        }
        let prefix = b"error: ";
        let mut body = Vec::new();
        let traces = self.traces.borrow();
        if !traces.is_empty() {
            // `tracesSeen` is never cleared: Nix passes it by value to
            // the function that would clear it.
            let mut seen: BTreeSet<TraceKey> = BTreeSet::new();
            let key_of = |t: &Trace| -> TraceKey {
                (
                    t.pos
                        .as_ref()
                        .map(|p| (p.info.origin.display(), p.line, p.column)),
                    t.hint.clone(),
                )
            };
            let mut skipped: Vec<&Trace> = Vec::new();
            let mut count = 0usize;
            let mut truncate = false;
            for trace in traces.iter().rev() {
                if trace.hint.is_empty() {
                    continue;
                }
                if !show_trace && count > 3 {
                    truncate = true;
                }
                if !truncate || trace.always {
                    let key = key_of(trace);
                    if seen.contains(&key) {
                        skipped.push(trace);
                        continue;
                    }
                    seen.insert(key);
                    print_skipped(&mut body, &mut count, &mut skipped);
                    count += 1;
                    print_trace(&mut body, &mut count, trace);
                }
            }
            print_skipped(&mut body, &mut count, &mut skipped);
            if truncate {
                body.extend_from_slice(
                    b"\n(stack trace truncated; use '--show-trace' to show the full, detailed trace)\n",
                );
            }
            body.push(b'\n');
            body.extend_from_slice(prefix);
        }
        body.extend_from_slice(&self.msg);
        body.push(b'\n');
        print_pos(&mut body, b"", self.pos.borrow().as_ref());
        let suggestions = self.suggestions.borrow();
        if !suggestions.is_empty() {
            body.extend_from_slice(b"Did you mean ");
            body.extend_from_slice(&format_suggestions_bytes(&suggestions));
            body.extend_from_slice(b"?\n");
        }
        indent(prefix, &[b' '; 7], chomp(&body)).into()
    }
}

impl std::fmt::Display for EvalError {
    /// The rendering with traces, as UTF-8 text (an ill-formed sequence
    /// shows as U+FFFD): for logs and test failures, not for printing
    /// as Nix does, which [`EvalError::render`] is for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.render(true)))
    }
}

impl std::error::Error for EvalError {}

/// `printTrace`.
fn print_trace(out: &mut Vec<u8>, count: &mut usize, trace: &Trace) {
    out.extend_from_slice("\n… ".as_bytes());
    out.extend_from_slice(&trace.hint);
    out.push(b'\n');
    if print_pos(out, b"  ", trace.pos.as_ref()) {
        *count += 1;
    }
}

/// `printSkippedTracesMaybe`.
fn print_skipped(out: &mut Vec<u8>, count: &mut usize, skipped: &mut Vec<&Trace>) {
    if !skipped.is_empty() {
        if skipped.len() <= 5 {
            for trace in skipped.iter() {
                print_trace(out, count, trace);
            }
        } else {
            out.extend_from_slice(
                format!("\n({} duplicate frames omitted)\n", skipped.len()).as_bytes(),
            );
        }
    }
    skipped.clear();
}

/// `printPosMaybe`: `at <pos>:` and the lines of code around it (when
/// the text can be read), or nothing for an unknown position. Returns
/// whether there was a position.
fn print_pos(out: &mut Vec<u8>, indent: &[u8], pos: Option<&Position>) -> bool {
    let Some(p) = pos.filter(|p| p.line > 0) else {
        return false;
    };
    out.extend_from_slice(indent);
    out.extend_from_slice(b"at ");
    out.extend_from_slice(&p.display());
    // `getCodeLines`: nothing for a file whose text cannot be read.
    if p.info.source().is_some() {
        out.push(b':');
        source::print_code_lines(out, p.line, p.column, |n| p.info.line_text(n));
        out.push(b'\n');
    }
    true
}

/// `Suggestions::to_string` after `trim()`: `a`, or `one of a, b or c`,
/// each name in Nix's colour (front ends filter the codes out with
/// [`crate::filter_ansi_escapes`] where Nix's logger would). Each name
/// goes through `filterANSIEscapes` on its own, so a tab in a name is
/// expanded relative to the start of the name. Names are bytes, as
/// symbols are; [`format_suggestions`] is the same for text.
pub fn format_suggestions_bytes<S: AsRef<[u8]>>(suggestions: &[S]) -> Vec<u8> {
    // The colour codes are kept (front ends strip them with
    // `filter_ansi_escapes`, as Nix's logger does) because they are
    // observable: a suggestion ending in whitespace is not chomped when a
    // code follows it.
    let mut out = Vec::new();
    if suggestions.len() > 1 {
        out.extend_from_slice(b"one of ");
    }
    for (i, s) in suggestions.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(if i == suggestions.len() - 1 {
                b" or "
            } else {
                b", "
            });
        }
        out.extend_from_slice(b"\x1b[35;1m");
        out.extend_from_slice(&crate::terminal::filter_ansi_escapes(
            s.as_ref(),
            true,
            usize::MAX,
        ));
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

/// [`format_suggestions_bytes`] for names that are text.
pub fn format_suggestions(suggestions: &[String]) -> String {
    String::from_utf8(format_suggestions_bytes(suggestions)).expect("filtering text keeps it text")
}

/// Levenshtein-based `Suggestions::bestMatches(...).trim()`: the closest
/// names (by distance, then name), at most five, within distance two.
/// Names are bytes, as symbols are; [`best_suggestions`] is the same for
/// text.
pub fn best_suggestions_bytes<'n>(
    names: impl Iterator<Item = &'n [u8]>,
    query: &[u8],
) -> Vec<BString> {
    scored_suggestions_bytes(names, query)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

/// [`best_suggestions_bytes`] for names that are text.
pub fn best_suggestions<'n>(names: impl Iterator<Item = &'n str>, query: &str) -> Vec<String> {
    scored_suggestions(names, query)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

/// [`best_suggestions_bytes`] with each name's distance: what Nix's
/// `Suggestions` holds, for joining the suggestions of several lookups
/// (`operator+=`, a set ordered by distance, then name) before trimming
/// them again.
pub fn scored_suggestions_bytes<'n>(
    names: impl Iterator<Item = &'n [u8]>,
    query: &[u8],
) -> Vec<(usize, BString)> {
    let mut scored: Vec<(usize, &[u8])> = names.map(|n| (levenshtein(query, n), n)).collect();
    scored.sort();
    scored.dedup();
    scored
        .into_iter()
        .filter(|&(d, _)| d <= 2)
        .take(5)
        .map(|(d, n)| (d, BString::from(n)))
        .collect()
}

/// [`scored_suggestions_bytes`] for names that are text.
pub fn scored_suggestions<'n>(
    names: impl Iterator<Item = &'n str>,
    query: &str,
) -> Vec<(usize, String)> {
    scored_suggestions_bytes(names.map(str::as_bytes), query.as_bytes())
        .into_iter()
        .map(|(d, n)| {
            let n = String::from_utf8(n.into()).expect("one of the names, which are text");
            (d, n)
        })
        .collect()
}

fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    let mut v0: Vec<usize> = (0..=b.len()).collect();
    let mut v1 = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        v1[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let deletion = v0[j + 1] + 1;
            let insertion = v1[j] + 1;
            let substitution = if ca == cb { v0[j] } else { v0[j] + 1 };
            v1[j + 1] = deletion.min(insertion).min(substitution);
        }
        std::mem::swap(&mut v0, &mut v1);
    }
    v0[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(origin: Origin, src: &[u8]) -> Rc<SourceInfo> {
        Rc::new(SourceInfo::new(origin, src.to_vec()))
    }

    fn render(e: &EvalError) -> String {
        String::from_utf8(e.render(true).into()).expect("UTF-8 in these tests")
    }

    #[test]
    fn line_and_column_are_one_based_bytes() {
        let i = info(Origin::String, b"ab\ncd\r\nef");
        assert_eq!(i.line_col(0), (1, 1));
        assert_eq!(i.line_col(1), (1, 2));
        assert_eq!(i.line_col(3), (2, 1));
        assert_eq!(i.line_col(7), (3, 1));
        assert_eq!(i.line_text(2), Some(&b"cd"[..]));
        assert_eq!(i.line_text(4), None);
        let j = info(Origin::String, b"a\nb\n");
        assert_eq!(j.line_col(4), (3, 1));
        assert_eq!(j.line_text(3), Some(&b""[..]));
    }

    #[test]
    fn command_line_sources_end_at_their_first_nul() {
        let i = info(Origin::Stdin, b"a\n\0b\nc");
        assert_eq!(i.line_text(2), Some(&b""[..]));
        assert_eq!(i.line_text(3), None);
        assert_eq!(i.line_col(6), (2, 5));
        let file = info(
            Origin::File {
                display: "/f.nix".into(),
                path: "/f.nix".into(),
            },
            b"a\n\0b\nc",
        );
        assert_eq!(file.line_text(3), Some(&b"c"[..]));
    }

    #[test]
    fn renders_like_nix() {
        let i = info(Origin::String, b"let f = x: x; in f 1 2");
        let e = EvalError::new(
            ErrorKind::TypeError,
            "attempt to call something which is not a function but an integer: 1",
        )
        .at(Some(Position {
            info: i,
            line: 1,
            column: 18,
        }));
        assert_eq!(
            render(&e),
            "error: attempt to call something which is not a function but an integer: 1\n       at «string»:1:18:\n            1| let f = x: x; in f 1 2\n             |                  ^"
        );
    }

    #[test]
    fn renders_bytes_as_they_are() {
        let i = info(Origin::String, b"throw \"\xff\"");
        let e = EvalError::new(ErrorKind::Thrown, &b"\xff"[..]).at(Some(Position {
            info: i,
            line: 1,
            column: 1,
        }));
        assert_eq!(
            e.render(true),
            b"error: \xff\n       at \xC2\xABstring\xC2\xBB:1:1:\n            1| throw \"\xff\"\n             | ^"
        );
    }

    /// A file's name in a position and in a trace's position is its
    /// bytes: `/pwd/caf\xe9.nix`, not a replacement character.
    #[test]
    fn a_file_name_that_is_not_utf8_is_shown_as_its_bytes() {
        let i = info(
            Origin::File {
                display: (&b"/pwd/caf\xe9.nix"[..]).into(),
                path: (&b"/pwd/caf\xe9.nix"[..]).into(),
            },
            b"1 + { }",
        );
        let at = Position {
            info: i,
            line: 1,
            column: 5,
        };
        assert_eq!(at.display(), &b"/pwd/caf\xe9.nix:1:5"[..]);
        let e = EvalError::new(ErrorKind::TypeError, "cannot add a set to an integer")
            .at(Some(at.clone()));
        e.add_trace(Some(at), "while adding");
        assert_eq!(
            e.render(true),
            &b"error:\n       \xe2\x80\xa6 while adding\n         at /pwd/caf\xe9.nix:1:5:\n            1| 1 + { }\n             |     ^\n\n       error: cannot add a set to an integer\n       at /pwd/caf\xe9.nix:1:5:\n            1| 1 + { }\n             |     ^"[..]
        );
    }

    #[test]
    fn an_empty_source_is_an_empty_file() {
        let e = EvalError::new(ErrorKind::Parse, "syntax error, unexpected end of file").at(Some(
            Position {
                info: info(Origin::String, b""),
                line: 1,
                column: 1,
            },
        ));
        assert_eq!(
            render(&e),
            "error: syntax error, unexpected end of file\n       at «string»:1:1:\n            1| (empty file)"
        );
    }

    #[test]
    fn renders_traces_outermost_first() {
        let i = info(Origin::String, b"builtins.substring 0 1 { }");
        let e = EvalError::new(ErrorKind::TypeError, "cannot coerce a set to a string: { }");
        e.add_trace(
            None,
            "while evaluating the third argument (the string) passed to builtins.substring",
        );
        e.add_trace(
            Some(Position {
                info: i.clone(),
                line: 1,
                column: 1,
            }),
            "while calling the 'substring' builtin",
        );
        assert_eq!(
            render(&e),
            "error:\n       … while calling the 'substring' builtin\n         at «string»:1:1:\n            1| builtins.substring 0 1 { }\n             | ^\n\n       … while evaluating the third argument (the string) passed to builtins.substring\n\n       error: cannot coerce a set to a string: { }"
        );
    }

    #[test]
    fn suggestions() {
        let names = ["a", "abc", "zzz"];
        assert_eq!(best_suggestions(names.into_iter(), "b"), vec!["a", "abc"]);
        assert_eq!(best_suggestions(names.into_iter(), "zzzz"), vec!["zzz"]);
        assert_eq!(best_suggestions(names.into_iter(), "ab"), vec!["a", "abc"]);
        assert_eq!(
            crate::terminal::filter_ansi_escapes_str(
                &format_suggestions(&["a".into(), "abc".into()]),
                true,
                usize::MAX
            ),
            "one of a or abc"
        );
        // A name ending in whitespace keeps it: the colour code that
        // follows stops `chomp`.
        assert_eq!(
            format_suggestions(&["x ".into()]),
            "\u{1b}[35;1mx \u{1b}[0m"
        );
    }

    #[test]
    fn msg_joins_text_and_bytes() {
        let name: Vec<u8> = b"\xff".to_vec();
        assert_eq!(
            msg!("attribute '", name, "' missing"),
            b"attribute '\xff' missing"
        );
    }
}
