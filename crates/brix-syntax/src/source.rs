//! Source origins, the offset → line/column mapping and code excerpts.
//!
//! Positions in the AST are byte offsets ([`crate::ast::Pos`]). Nix turns them
//! into 1-based line and column numbers only when it needs to print them,
//! using a table of line start offsets (`PosTable`), and shows the lines
//! around a position in error messages (`printCodeLines`); this module does
//! the same, for the parser's diagnostics and for the evaluator's errors,
//! which keep the line table of each file they load ([`line_starts`],
//! [`line_col`], [`line_text`], [`print_code_lines`]).

use std::fmt;

use bstr::BString;

use crate::ast::Pos;

/// Where a source text came from, as Nix prints it in positions
/// (`Pos::Origin`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// No origin: printed as `«none»`.
    None,
    /// Standard input: printed as `«stdin»`.
    Stdin,
    /// An expression given on the command line: printed as `«string»`.
    String,
    /// A file, printed as its path: bytes, as Nix's paths are, printed
    /// as they are ([`Origin::as_bytes`]).
    Path(BString),
}

impl Origin {
    /// The origin as Nix prints it in a position: the bytes of a file's
    /// path as they are.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Origin::None => "«none»".as_bytes(),
            Origin::Stdin => "«stdin»".as_bytes(),
            Origin::String => "«string»".as_bytes(),
            Origin::Path(p) => p.as_slice(),
        }
    }
}

impl fmt::Display for Origin {
    /// Lossy for a path that is not UTF-8: Rust-side text only. What Nix
    /// prints is [`Origin::as_bytes`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::None => f.write_str("«none»"),
            Origin::Stdin => f.write_str("«stdin»"),
            Origin::String => f.write_str("«string»"),
            Origin::Path(p) => fmt::Display::fmt(p, f),
        }
    }
}

/// The text Nix reads positions and code lines from (`Pos::getSource`).
/// It keeps standard input and command-line expressions as C strings, so
/// their text ends at the first NUL; a file's is all of it.
pub fn code_text<'a>(origin: &Origin, src: &'a [u8]) -> &'a [u8] {
    match origin {
        Origin::Stdin | Origin::String => cut_at_nul(src),
        Origin::None | Origin::Path(_) => src,
    }
}

/// `src` up to its first NUL (`std::string(s.c_str())`).
pub fn cut_at_nul(src: &[u8]) -> &[u8] {
    match src.iter().position(|&b| b == 0) {
        Some(nul) => &src[..nul],
        None => src,
    }
}

/// The byte offsets at which the lines of `text` start (`PosTable`'s
/// line cache): lines end at `\n`, `\r` or `\r\n`, and a terminator at
/// the very end starts one more, empty line (`Pos::LinesIterator`). An
/// empty text has no lines, but its table still holds the start of the
/// first, as Nix's does, so that every offset has a line.
pub fn line_starts(text: &[u8]) -> Vec<u32> {
    let offset = |i: usize| u32::try_from(i).expect("source texts are smaller than 4 GiB");
    let mut starts = vec![0];
    let mut i = 0;
    while let Some(eol) = text[i..].iter().position(|&b| b == b'\n' || b == b'\r') {
        i += eol;
        if text[i] == b'\r' {
            i += 1;
        }
        if text.get(i) == Some(&b'\n') {
            i += 1;
        }
        starts.push(offset(i));
    }
    starts
}

/// The 1-based line and column (in bytes) of a byte offset, given the
/// text's [`line_starts`] (`PosTable::operator[]`). An offset past the end
/// of the text is on its last line.
pub fn line_col(starts: &[u32], offset: u32) -> (u32, u32) {
    let line = starts.partition_point(|&start| start <= offset).max(1);
    (line as u32, offset - starts[line - 1] + 1)
}

/// The text of 1-based line `line` without its terminator, as
/// `Pos::LinesIterator` yields it: `None` past the last line, and for
/// every line of an empty text. `None` too when `starts` does not belong
/// to `text` (a line starting past its end).
pub fn line_text<'t>(text: &'t [u8], starts: &[u32], line: u32) -> Option<&'t [u8]> {
    if text.is_empty() {
        return None;
    }
    let start = *starts.get(line.checked_sub(1)? as usize)? as usize;
    let rest = text.get(start..)?;
    let end = rest
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `printCodeLines` (`error.cc`) with an empty prefix: the line before
/// `line`, `line` itself with a caret under `column` (when there is one),
/// and the line after, each on a line of its own, appended to `out` after
/// a newline. `lines` gives the text of a 1-based line ([`line_text`]).
/// A line that does not exist is left out, except the error's own, which
/// shows as `(empty file)`. Colours are left out; the caller chomps
/// trailing whitespace off each line when it indents them.
pub fn print_code_lines<'t>(
    out: &mut Vec<u8>,
    line: u32,
    column: u32,
    lines: impl Fn(u32) -> Option<&'t [u8]>,
) {
    let code_line = |out: &mut Vec<u8>, n: u32, text: &[u8]| {
        out.extend_from_slice(format!("\n {n:>5}| ").as_bytes());
        out.extend_from_slice(text);
    };
    if line > 1 {
        if let Some(prev) = lines(line - 1) {
            code_line(out, line - 1, prev);
        }
    }
    match lines(line) {
        Some(text) => {
            code_line(out, line, text);
            if column > 0 {
                out.extend_from_slice(b"\n      |");
                out.resize(out.len() + column as usize, b' ');
                out.push(b'^');
            }
        }
        None => out.extend_from_slice(format!("\n {line:>5}| (empty file)").as_bytes()),
    }
    if let Some(next) = lines(line + 1) {
        code_line(out, line + 1, next);
    }
}

/// A source text with its line table, as Nix reads it for an origin
/// ([`code_text`]).
pub struct SourceFile<'a> {
    text: &'a [u8],
    /// Byte offset at which each line starts ([`line_starts`]).
    line_starts: Vec<u32>,
}

impl<'a> SourceFile<'a> {
    /// The text of `src` from `origin` ([`code_text`]) and its line table.
    pub fn new(origin: &Origin, src: &'a [u8]) -> Self {
        let text = code_text(origin, src);
        SourceFile {
            text,
            line_starts: line_starts(text),
        }
    }

    /// The text positions refer to: the source, cut at a NUL for stdin
    /// and command-line sources.
    pub fn text(&self) -> &'a [u8] {
        self.text
    }

    /// 1-based line and column (in bytes) of a byte offset.
    pub fn line_col(&self, pos: Pos) -> (u32, u32) {
        line_col(&self.line_starts, pos.0)
    }

    /// The number of entries in the line table: the lines, counting the
    /// empty line after a final newline (1 for an empty text).
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The text of a 1-based line, without its terminator ([`line_text`]).
    pub fn line(&self, line: u32) -> Option<&'a [u8]> {
        line_text(self.text, &self.line_starts, line)
    }

    /// `origin:line:column`, as Nix prints a position: bytes, the
    /// origin's as they are.
    pub fn format_pos(&self, origin: &Origin, pos: Pos) -> Vec<u8> {
        let (line, col) = self.line_col(pos);
        let mut out = origin.as_bytes().to_vec();
        out.extend_from_slice(format!(":{line}:{col}").as_bytes());
        out
    }

    /// [`print_code_lines`] for a position in this text.
    pub fn print_code_lines(&self, out: &mut Vec<u8>, pos: Pos) {
        let (line, col) = self.line_col(pos);
        print_code_lines(out, line, col, |n| self.line(n));
    }

    /// The text from `from` up to (not including) `to`, its lines joined
    /// with `\n` whatever their terminators were; `None` when `to` is on
    /// an earlier line (`Pos::getSnippetUpTo`).
    pub fn snippet(&self, from: Pos, to: Pos) -> Option<Vec<u8>> {
        let (first_line, first_col) = self.line_col(from);
        let (last_line, last_col) = self.line_col(to);
        if last_line < first_line {
            return None;
        }
        let mut out = Vec::new();
        for n in first_line..=last_line {
            let text = self.line(n).unwrap_or_default();
            let start = if n == first_line {
                (first_col as usize - 1).min(text.len())
            } else {
                0
            };
            let end = if n == last_line {
                (last_col as usize - 1).max(start).min(text.len())
            } else {
                text.len()
            };
            out.extend_from_slice(&text[start..end]);
            if n != last_line {
                out.push(b'\n');
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(src: &[u8]) -> SourceFile<'_> {
        SourceFile::new(&Origin::Path("/f.nix".into()), src)
    }

    #[test]
    fn lines_count_the_empty_line_after_a_final_newline() {
        let f = file(b"a\nb\n");
        assert_eq!(f.line_count(), 3);
        assert_eq!(f.line(1), Some(&b"a"[..]));
        assert_eq!(f.line(2), Some(&b"b"[..]));
        assert_eq!(f.line(3), Some(&b""[..]));
        assert_eq!(f.line(4), None);
        assert_eq!(f.line_col(Pos(4)), (3, 1));
    }

    #[test]
    fn no_final_newline() {
        let f = file(b"a\nbc");
        assert_eq!(f.line_count(), 2);
        assert_eq!(f.line_col(Pos(3)), (2, 2));
        assert_eq!(f.line_col(Pos(4)), (2, 3));
    }

    #[test]
    fn crlf_and_bare_cr() {
        let f = file(b"a\r\nb\rc");
        assert_eq!(f.line_count(), 3);
        assert_eq!(f.line(2), Some(&b"b"[..]));
        assert_eq!(f.line_col(Pos(5)), (3, 1));
        // `\n\r` is two terminators.
        assert_eq!(line_starts(b"a\n\rb"), [0, 2, 3]);
    }

    #[test]
    fn an_empty_file_has_no_lines_but_every_offset_has_a_position() {
        let f = file(b"");
        assert_eq!(f.line_count(), 1);
        assert_eq!(f.line_col(Pos(0)), (1, 1));
        assert_eq!(f.line(1), None);
    }

    #[test]
    fn stdin_and_string_sources_end_at_the_first_nul() {
        let src = b"a\n\0b\nc";
        let stdin = SourceFile::new(&Origin::Stdin, src);
        assert_eq!(stdin.text(), b"a\n");
        assert_eq!(stdin.line(2), Some(&b""[..]));
        assert_eq!(stdin.line(3), None);
        // An offset past the NUL is on the last line of what is left.
        assert_eq!(stdin.line_col(Pos(5)), (2, 4));
        assert_eq!(SourceFile::new(&Origin::String, b"\0").line(1), None);
        assert_eq!(file(src).line(3), Some(&b"c"[..]));
    }

    #[test]
    fn positions_show_the_path_as_its_bytes() {
        let origin = Origin::Path(b"/d\xe9/caf\xe9.nix".as_slice().into());
        let f = SourceFile::new(&origin, b"a\nbc");
        assert_eq!(f.format_pos(&origin, Pos(3)), b"/d\xe9/caf\xe9.nix:2:2");
        assert_eq!(origin.as_bytes(), b"/d\xe9/caf\xe9.nix");
        assert_eq!(origin.to_string(), "/d\u{fffd}/caf\u{fffd}.nix");
        assert_eq!(
            f.format_pos(&Origin::Stdin, Pos(0)),
            "«stdin»:1:1".as_bytes()
        );
        assert_eq!(Origin::String.as_bytes(), "«string»".as_bytes());
        assert_eq!(Origin::None.as_bytes(), "«none»".as_bytes());
    }

    #[test]
    fn code_lines_like_nix() {
        let f = file(b"one\ntwo\nthree");
        let mut out = Vec::new();
        f.print_code_lines(&mut out, Pos(5));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\n     1| one\n     2| two\n      |  ^\n     3| three"
        );
        let mut out = Vec::new();
        file(b"").print_code_lines(&mut out, Pos(0));
        assert_eq!(String::from_utf8(out).unwrap(), "\n     1| (empty file)");
        // A line that is not UTF-8 is copied as it is.
        let mut out = Vec::new();
        file(b"\xff").print_code_lines(&mut out, Pos(0));
        assert_eq!(out, b"\n     1| \xff\n      | ^");
    }
}
