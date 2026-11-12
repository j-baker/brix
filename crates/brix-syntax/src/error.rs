//! Parse errors and warnings, and their rendering in Nix's format, with
//! the layout helpers (`indent`, `chomp`) the evaluator's errors use too.

use std::fmt;

use bstr::BString;

use crate::ast::Pos;
use crate::source::{Origin, SourceFile};

/// A parse error: Nix's `ParseError` (or the `Error` the parser throws for
/// `~` paths in pure mode, which carries no position).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The message, without the `error: ` prefix, and without the
    /// position of [`ParseError::defined_at`]. Bytes: it can quote a
    /// name or a string from the source, which need not be UTF-8.
    pub msg: BString,
    /// Where the error was detected, if known.
    pub pos: Option<Pos>,
    /// For `dupAttr`'s "attribute 'a' already defined": where the
    /// attribute was defined first. Nix's message ends with
    /// ` at <origin>:<line>:<column>` of this position; it is kept apart
    /// so that whoever shows the error supplies the origin (a compiled
    /// image is loaded from somewhere else than it was parsed).
    pub defined_at: Option<Pos>,
}

impl ParseError {
    pub fn new(msg: impl Into<BString>, pos: Pos) -> Self {
        ParseError {
            msg: msg.into(),
            pos: Some(pos),
            defined_at: None,
        }
    }

    /// The message as Nix prints it for a source with this origin, the
    /// position of [`ParseError::defined_at`] included.
    pub fn message(&self, origin: &Origin, src: &[u8]) -> Vec<u8> {
        let mut msg = self.msg.to_vec();
        if let Some(at) = self.defined_at {
            msg.extend_from_slice(b" at ");
            msg.extend_from_slice(&SourceFile::new(origin, src).format_pos(origin, at));
        }
        msg
    }

    /// Renders the error as `nix` prints it to stderr, including the
    /// position and the lines of code around it, with a trailing newline:
    ///
    /// ```text
    /// error: attribute 'x' already defined at «stdin»:1:3
    ///        at «stdin»:3:3:
    ///             2|   y = 456;
    ///             3|   x = 789;
    ///              |   ^
    ///             4| }
    /// ```
    ///
    /// (`showErrorInfo` and `printCodeLines` in Nix's `error.cc`.) The
    /// lines of code are the source's bytes, UTF-8 or not.
    pub fn render(&self, origin: &Origin, src: &[u8]) -> Vec<u8> {
        let mut out = render_diagnostic("error", &self.message(origin, src), self.pos, origin, src);
        out.push(b'\n');
        out
    }
}

impl fmt::Display for ParseError {
    /// The message, with the earlier definition as a byte offset: the
    /// origin needed to show it as Nix does is not known here (see
    /// [`ParseError::message`]). Bytes that are not UTF-8 are shown as
    /// U+FFFD.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)?;
        match self.defined_at {
            Some(at) => write!(f, " at byte {}", at.0),
            None => Ok(()),
        }
    }
}

impl std::error::Error for ParseError {}

/// A warning Nix's parser logs. Parsing goes on after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Warning {
    /// A `lint-*` setting at `warn` (see [`crate::Diagnose`]): the error
    /// the setting raises at `fatal`, logged as a warning instead
    /// (`diagnose`, which shows it with its position and code lines).
    Lint { msg: String, pos: Pos },
    /// `ExprCall::warnIfCursedOr`: `or` used as an identifier argument
    /// (`f or`) where the expression is not the first in an application,
    /// whose meaning a future Nix will change. `pos` is the call's
    /// position, `end` the end of the `or`.
    CursedOr { pos: Pos, end: Pos },
}

impl Warning {
    /// The warning as `nix` prints it to stderr, `warning: ` prefix
    /// included and without a final newline: one logged message, whose
    /// quoted code is the source's bytes.
    pub fn render(&self, origin: &Origin, src: &[u8]) -> Vec<u8> {
        match self {
            Warning::Lint { msg, pos } => {
                render_diagnostic("warning", msg.as_bytes(), Some(*pos), origin, src)
            }
            Warning::CursedOr { pos, end } => {
                // A plain `warn(…)`: continuation lines are not indented.
                let file = SourceFile::new(origin, src);
                let mut out = b"warning: at ".to_vec();
                out.extend_from_slice(&file.format_pos(origin, *pos));
                out.extend_from_slice(
                    b": This expression uses `or` as an identifier in a way that will change in a future Nix release.\n\
                      Wrap this entire expression in parentheses to preserve its current meaning:\n    (",
                );
                match file.snippet(*pos, *end) {
                    Some(snippet) => out.extend_from_slice(&snippet),
                    None => out.extend_from_slice(b"could not read expression"),
                }
                out.extend_from_slice(
                    b")\nGive feedback at https://github.com/NixOS/nix/pull/11121",
                );
                out
            }
        }
    }
}

/// Lays out a diagnostic as Nix's `showErrorInfo` does: `<level>: <msg>`,
/// then the position and the lines of code around it (`printPosMaybe`),
/// every further line indented to the width of `<level>: ` and chomped.
/// No final newline.
fn render_diagnostic(
    level: &str,
    msg: &[u8],
    pos: Option<Pos>,
    origin: &Origin,
    src: &[u8],
) -> Vec<u8> {
    let mut body = msg.to_vec();
    body.push(b'\n');
    if let Some(pos) = pos {
        let file = SourceFile::new(origin, src);
        body.extend_from_slice(b"at ");
        body.extend_from_slice(&file.format_pos(origin, pos));
        body.push(b':');
        file.print_code_lines(&mut body, pos);
        body.push(b'\n');
    }
    let prefix = format!("{level}: ");
    indent(prefix.as_bytes(), &prefix_spaces(&prefix), chomp(&body))
}

/// As many spaces as `prefix` has bytes: the indentation of a message's
/// further lines (`prefix` is ASCII, as every prefix Nix uses is).
fn prefix_spaces(prefix: &str) -> Vec<u8> {
    vec![b' '; prefix.len()]
}

/// `chomp` (`strings.cc`): `s` without trailing spaces, tabs, carriage
/// returns and newlines.
pub fn chomp(s: &[u8]) -> &[u8] {
    let end = s
        .iter()
        .rposition(|c| !matches!(c, b' ' | b'\n' | b'\r' | b'\t'))
        .map_or(0, |i| i + 1);
    &s[..end]
}

/// `indent` (`error.cc`): every line of `s` after `first` (the first) or
/// `rest` (the others), each chomped, joined by newlines. A final newline
/// starts no further line, and an empty `s` has no lines at all.
pub fn indent(first: &[u8], rest: &[u8], s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + first.len());
    if s.is_empty() {
        return out;
    }
    let s = s.strip_suffix(b"\n").unwrap_or(s);
    for (i, line) in s.split(|&c| c == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let start = out.len();
        out.extend_from_slice(if i == 0 { first } else { rest });
        out.extend_from_slice(line);
        let kept = chomp(&out[start..]).len();
        out.truncate(start + kept);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rendered: Vec<u8>) -> String {
        String::from_utf8(rendered).expect("UTF-8 in these tests")
    }

    #[test]
    fn renders_like_nix() {
        let src = b"{\n  x = 123;\n  y = 456;\n  x = 789;\n}\n";
        let err = ParseError {
            msg: "attribute 'x' already defined".into(),
            pos: Some(Pos(26)),
            defined_at: Some(Pos(4)),
        };
        assert_eq!(
            text(err.render(&Origin::Stdin, src)),
            "error: attribute 'x' already defined at «stdin»:2:3\n       at «stdin»:4:3:\n            3|   y = 456;\n            4|   x = 789;\n             |   ^\n            5| }\n"
        );
        // The earlier definition is shown in the origin the error is
        // rendered for.
        assert_eq!(
            err.message(&Origin::Path("/src/default.nix".into()), src),
            b"attribute 'x' already defined at /src/default.nix:2:3"
        );
        assert_eq!(err.to_string(), "attribute 'x' already defined at byte 4");
    }

    #[test]
    fn renders_without_position() {
        let err = ParseError {
            msg: "the path '~/x' can not be resolved in pure mode".into(),
            pos: None,
            defined_at: None,
        };
        assert_eq!(
            text(err.render(&Origin::Stdin, b"~/x")),
            "error: the path '~/x' can not be resolved in pure mode\n"
        );
    }

    #[test]
    fn empty_line_has_no_trailing_space() {
        let err = ParseError::new("syntax error, unexpected end of file", Pos(10));
        assert_eq!(
            text(err.render(&Origin::Stdin, b"# comment\n")),
            "error: syntax error, unexpected end of file\n       at «stdin»:2:1:\n            1| # comment\n            2|\n             | ^\n"
        );
    }

    #[test]
    fn multi_line_messages_are_indented_and_stdin_sources_end_at_nul() {
        let err = ParseError::new("input string 'a\n␀' cannot be represented", Pos(1));
        assert_eq!(
            text(err.render(&Origin::Stdin, b"\"a\0b\"\n")),
            "error: input string 'a\n       ␀' cannot be represented\n       at «stdin»:1:2:\n            1| \"a\n             |  ^\n"
        );
    }

    #[test]
    fn lint_warnings_are_laid_out_like_errors() {
        let w = Warning::Lint {
            msg: "URL literals are discouraged. Consider using a string literal \"http://x\" instead (lint-url-literals)".into(),
            pos: Pos(2),
        };
        assert_eq!(
            text(w.render(&Origin::Stdin, b"[ http://x ]")),
            "warning: URL literals are discouraged. Consider using a string literal \"http://x\" instead (lint-url-literals)\n         at «stdin»:1:3:\n              1| [ http://x ]\n               |   ^"
        );
    }

    #[test]
    fn cursed_or_warnings_quote_the_expression() {
        let src = b"[\n  (x:\n x) or ]";
        let w = Warning::CursedOr {
            pos: Pos(4),
            end: Pos(14),
        };
        assert_eq!(
            text(w.render(&Origin::Path("/f.nix".into()), src)),
            "warning: at /f.nix:2:3: This expression uses `or` as an identifier in a way that will change in a future Nix release.\nWrap this entire expression in parentheses to preserve its current meaning:\n    ((x:\n x) or)\nGive feedback at https://github.com/NixOS/nix/pull/11121"
        );
    }

    #[test]
    fn empty_sources_show_as_empty_files() {
        let err = ParseError::new("syntax error, unexpected end of file", Pos(0));
        assert_eq!(
            text(err.render(&Origin::Stdin, b"")),
            "error: syntax error, unexpected end of file\n       at «stdin»:1:1:\n            1| (empty file)\n"
        );
        // Positions past the NUL are counted on the text before it.
        let err = ParseError::new("syntax error, unexpected end of file", Pos(1));
        assert_eq!(
            text(err.render(&Origin::Stdin, b"\0")),
            "error: syntax error, unexpected end of file\n       at «stdin»:1:2:\n            1| (empty file)\n"
        );
    }

    #[test]
    fn code_lines_keep_their_bytes() {
        let err = ParseError::new("syntax error", Pos(1));
        assert_eq!(
            err.render(&Origin::Stdin, b"\"\xff"),
            b"error: syntax error\n       at \xC2\xABstdin\xC2\xBB:1:2:\n            1| \"\xff\n             |  ^\n"
        );
    }

    /// A file's path is shown as its bytes in every position. Oracle:
    /// `nix-instantiate --parse d\xe9/caf\xe9.nix` (`tests/oracle.rs`).
    #[test]
    fn positions_show_a_file_path_as_its_bytes() {
        let origin = Origin::Path(b"/d\xe9/caf\xe9.nix".as_slice().into());
        let src = b"{ a = 1; a = 2; }";
        let err = ParseError {
            msg: "attribute 'a' already defined".into(),
            pos: Some(Pos(9)),
            defined_at: Some(Pos(2)),
        };
        assert_eq!(
            bstr::BStr::new(&err.render(&origin, src)),
            bstr::BStr::new(
                b"error: attribute 'a' already defined at /d\xe9/caf\xe9.nix:1:3\n       at /d\xe9/caf\xe9.nix:1:10:\n            1| { a = 1; a = 2; }\n             |          ^\n"
            )
        );
        let w = Warning::CursedOr {
            pos: Pos(0),
            end: Pos(1),
        };
        assert!(w
            .render(&origin, b"x or")
            .starts_with(b"warning: at /d\xe9/caf\xe9.nix:1:1: This expression"));
    }

    #[test]
    fn indent_and_chomp_like_nix() {
        assert_eq!(chomp(b"a \t\r\n"), b"a");
        assert_eq!(chomp(b" \n"), b"");
        assert_eq!(indent(b"> ", b"  ", b"a\n\nb \n"), b"> a\n\n  b");
        assert_eq!(indent(b"> ", b"  ", b"a\n"), b"> a");
        assert_eq!(indent(b"> ", b"  ", b"\n"), b">");
        assert_eq!(indent(b"> ", b"  ", b""), b"");
    }
}
