//! What Nix's logger does to every line it writes to stderr
//! (`filterANSIEscapes` in `terminal.cc`): strips ANSI escape sequences
//! (keeping colour codes only when `filter_all` is false), expands tabs to
//! eight-column stops, drops carriage returns and bells, and truncates to
//! `width` columns.
//!
//! The column counter runs over the whole message, newlines included: a
//! tab on the second line of a multi-line error message is expanded
//! relative to the total width so far, not to the start of its line. That
//! is what Nix does, and it is observable in `builtins.trace` output and
//! in error messages containing tabs, so the front ends must apply this
//! to every message exactly as Nix's `SimpleLogger::log` does: once per
//! logged message (one `trace:` line, or one whole rendered error).
//!
//! Messages are bytes: they quote values, which need not be UTF-8. Bytes
//! that do not form a character are copied as they are and count one
//! column each, as in Nix.

use unicode_width::UnicodeWidthChar;

/// `filterANSIEscapes(s, filterAll, width)`. Front ends call it with
/// `filter_all = true` (stderr is not a terminal) and `width =
/// usize::MAX`.
pub fn filter_ansi_escapes(s: &[u8], filter_all: bool, width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut w = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        match s[i] {
            0x1b => {
                let start = i;
                i += 1;
                let mut last = 0u8;
                match s.get(i) {
                    Some(b'[') => {
                        i += 1;
                        // Parameter bytes, intermediate bytes, final byte.
                        while s.get(i).is_some_and(|c| (0x30..=0x3f).contains(c)) {
                            i += 1;
                        }
                        while s.get(i).is_some_and(|c| (0x20..=0x2f).contains(c)) {
                            i += 1;
                        }
                        if let Some(&c) = s.get(i).filter(|c| (0x40..=0x7e).contains(*c)) {
                            last = c;
                            i += 1;
                        }
                    }
                    Some(b']') => {
                        // OSC: up to ESC or BEL, then a backslash after
                        // the ESC.
                        i += 1;
                        while s.get(i).is_some_and(|&c| c != 0x1b && c != 0x07) {
                            i += 1;
                        }
                        if let Some(&v) = s.get(i) {
                            i += 1;
                            if v == 0x1b && s.get(i) == Some(&b'\\') {
                                last = b'\\';
                                i += 1;
                            }
                        }
                    }
                    Some(c) if (0x40..=0x5f).contains(c) => i += 1,
                    _ => {}
                }
                if !filter_all && last == b'm' {
                    out.extend_from_slice(&s[start..i]);
                }
            }
            b'\t' => {
                i += 1;
                loop {
                    w += 1;
                    if w > width {
                        return out;
                    }
                    out.push(b' ');
                    if w % 8 == 0 {
                        break;
                    }
                }
            }
            b'\r' | 0x07 => i += 1,
            _ => {
                let (char_width, len) = char_width_utf8(&s[i..]);
                w += char_width;
                if w > width {
                    break;
                }
                out.extend_from_slice(&s[i..i + len]);
                i += len;
            }
        }
    }
    out
}

/// [`filter_ansi_escapes`] on text: the result is text too (escape
/// sequences, tabs and the width cut never split a character).
pub fn filter_ansi_escapes_str(s: &str, filter_all: bool, width: usize) -> String {
    String::from_utf8(filter_ansi_escapes(s.as_bytes(), filter_all, width))
        .expect("filtering removes or adds whole characters only")
}

/// `charWidthUTF8Helper`: the columns and the bytes of the character at
/// the start of `s` (not empty). A byte that cannot start a character is
/// one column and one byte; a sequence cut short is as many columns as
/// it has bytes. A decoded value that is not a character (a surrogate,
/// or past U+10FFFF) is as many columns as it has bytes too, as in Nix
/// (whose check against the encoding's range is meant to catch overlong
/// forms).
fn char_width_utf8(s: &[u8]) -> (usize, usize) {
    let lead = s[0];
    let (mut ch, len) = match lead {
        0x00..=0x7f => (u32::from(lead), 1),
        _ if lead & 0xe0 == 0xc0 => (u32::from(lead & 0x1f), 2),
        _ if lead & 0xf0 == 0xe0 => (u32::from(lead & 0x0f), 3),
        _ if lead & 0xf8 == 0xf0 => (u32::from(lead & 0x07), 4),
        _ => return (1, 1),
    };
    for i in 1..len {
        match s.get(i) {
            Some(&c) if c & 0xc0 == 0x80 => ch = (ch << 6) | u32::from(c & 0x3f),
            _ => return (i, i),
        }
    }
    match char::from_u32(ch) {
        Some(c) => (c.width().unwrap_or(0), len),
        None => (len, len),
    }
}

/// Lays out a logged message the way Nix's `showErrorInfo` does
/// (`error.cc`): `<prefix>: <first line>`, every further line indented to
/// the prefix's width, each line chomped of trailing whitespace and the
/// message chomped as a whole. Warnings from `builtins.warn` use the
/// prefix `evaluation warning`, the evaluator's own use `warning`.
pub fn indent_message(prefix: &str, message: impl AsRef<[u8]>) -> Vec<u8> {
    let first = format!("{prefix}: ");
    brix_syntax::error::indent(
        first.as_bytes(),
        &vec![b' '; first.len()],
        brix_syntax::error::chomp(message.as_ref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> String {
        filter_ansi_escapes_str(s, true, usize::MAX)
    }

    #[test]
    fn tabs_expand_to_eight_column_stops() {
        assert_eq!(f("trace: \t${"), "trace:  ${");
        assert_eq!(f("trace: bar\tü"), "trace: bar      ü");
        assert_eq!(f("\t"), "        ");
    }

    #[test]
    fn the_column_runs_across_lines() {
        // 3 columns, newline (width 0), then a tab: 5 more to reach 8.
        assert_eq!(f("abc\n\tx"), "abc\n     x");
    }

    #[test]
    fn escapes_are_stripped_and_controls_dropped() {
        assert_eq!(f("\u{1b}[31mred\u{1b}[0m\r\u{7}"), "red");
        assert_eq!(
            filter_ansi_escapes_str("\u{1b}[31mred\u{1b}[0m", false, usize::MAX),
            "\u{1b}[31mred\u{1b}[0m"
        );
        assert_eq!(f("\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{7}"), "link");
    }

    #[test]
    fn width_truncates() {
        assert_eq!(filter_ansi_escapes_str("abcdef", true, 3), "abc");
        assert_eq!(filter_ansi_escapes_str("日本語", true, 4), "日本");
    }

    #[test]
    fn bytes_that_are_not_utf8_are_kept_and_count_one_column_each() {
        let filter = |s: &[u8]| filter_ansi_escapes(s, true, usize::MAX);
        assert_eq!(filter(b"\xff\t|"), b"\xff       |");
        // A sequence cut short by a tab: its lead byte is one column.
        assert_eq!(filter(b"\xc3\t|"), b"\xc3       |");
        assert_eq!(filter(b"\xe2\x82\t|"), b"\xe2\x82      |");
        assert_eq!(filter_ansi_escapes(b"\xff\xfeab", true, 2), b"\xff\xfe");
    }

    #[test]
    fn multi_line_messages_align_under_the_prefix() {
        let indented =
            |prefix, message: &str| String::from_utf8(indent_message(prefix, message)).unwrap();
        assert_eq!(indented("warning", "one"), "warning: one");
        assert_eq!(
            indented("evaluation warning", "first,\n  second \n"),
            format!("evaluation warning: first,\n{}  second", " ".repeat(20))
        );
        // A blank continuation line stays blank rather than indented.
        assert_eq!(
            indented("warning", "a\n\nb"),
            format!("warning: a\n\n{}b", " ".repeat(9))
        );
        assert_eq!(indent_message("warning", b"\xff"), b"warning: \xff");
    }
}
