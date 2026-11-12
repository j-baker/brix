//! Indentation stripping for `''…''` strings (`stripIndentation` in
//! `parser-state.hh`).

use crate::ast::{Expr, Pos};

/// A piece of an indented string as the lexer delivers it.
pub(crate) enum IndPart {
    Str {
        text: Vec<u8>,
        has_indentation: bool,
    },
    Expr(Expr),
}

/// Removes the common indentation from the pieces of an indented string and
/// builds the resulting expression: a plain string when nothing is
/// interpolated, otherwise a `ConcatStrings` with `force_string` set.
///
/// The rules, as in Nix:
/// - the minimum indentation is the smallest number of leading spaces on a
///   line that has something other than spaces on it; interpolations and
///   escape pieces count as "something";
/// - whitespace-only lines (including the final one) do not influence it;
/// - that many spaces are dropped from the start of every line, but no
///   more than 1000000 (the starting value of Nix's minimum, which stays
///   in force when no line has anything on it);
/// - in the last piece, a final line consisting only of spaces is removed.
pub(crate) fn strip_indentation(pos: Pos, parts: Vec<(Pos, IndPart)>) -> Expr {
    if parts.is_empty() {
        return empty_string(pos);
    }

    let mut at_start_of_line = true;
    // Nix's starting value: when every line is blank, at most this many
    // spaces are dropped from each.
    let mut min_indent = 1_000_000;
    let mut cur_indent = 0;
    for (_, part) in &parts {
        match part {
            IndPart::Str {
                text,
                has_indentation: true,
            } => {
                for &b in text {
                    if at_start_of_line {
                        if b == b' ' {
                            cur_indent += 1;
                        } else if b == b'\n' {
                            cur_indent = 0;
                        } else {
                            at_start_of_line = false;
                            min_indent = min_indent.min(cur_indent);
                        }
                    } else if b == b'\n' {
                        at_start_of_line = true;
                        cur_indent = 0;
                    }
                }
            }
            _ => {
                if at_start_of_line {
                    at_start_of_line = false;
                    min_indent = min_indent.min(cur_indent);
                }
            }
        }
    }

    let n_parts = parts.len();
    let mut out: Vec<(Pos, Expr)> = Vec::with_capacity(n_parts);
    at_start_of_line = true;
    let mut cur_dropped = 0;
    for (i, (part_pos, part)) in parts.into_iter().enumerate() {
        match part {
            IndPart::Expr(e) => {
                at_start_of_line = false;
                cur_dropped = 0;
                out.push((part_pos, e));
            }
            IndPart::Str { text, .. } => {
                let mut s2 = Vec::with_capacity(text.len());
                for b in text {
                    if at_start_of_line {
                        if b == b' ' {
                            if cur_dropped >= min_indent {
                                s2.push(b);
                            }
                            cur_dropped += 1;
                        } else if b == b'\n' {
                            cur_dropped = 0;
                            s2.push(b);
                        } else {
                            at_start_of_line = false;
                            cur_dropped = 0;
                            s2.push(b);
                        }
                    } else {
                        s2.push(b);
                        if b == b'\n' {
                            at_start_of_line = true;
                        }
                    }
                }
                // Remove the last line if it consists only of spaces, but
                // only in the last piece (Nix's `n == 1` counts down the
                // pieces left).
                let remaining = n_parts - i;
                if remaining == 1 {
                    if let Some(p) = s2.iter().rposition(|&b| b == b'\n') {
                        if s2[p + 1..].iter().all(|&b| b == b' ') {
                            s2.truncate(p + 1);
                        }
                    }
                }
                if !s2.is_empty() {
                    out.push((
                        part_pos,
                        Expr::String {
                            pos: part_pos,
                            value: s2,
                        },
                    ));
                }
            }
        }
    }

    if out.is_empty() {
        return empty_string(pos);
    }
    if out.len() == 1 && matches!(out[0].1, Expr::String { .. }) {
        return out.pop().expect("one element").1;
    }
    Expr::ConcatStrings {
        pos,
        force_string: true,
        parts: out,
    }
}

fn empty_string(pos: Pos) -> Expr {
    Expr::String {
        pos,
        value: Vec::new(),
    }
}
