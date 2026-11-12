//! Regular expressions for `builtins.match` and `builtins.split`.
//!
//! Nix uses `std::regex` with `std::regex::extended` (POSIX extended
//! regular expressions, as libstdc++ reads them). The `regex` crate's
//! syntax is close to ERE; the differences that matter are translated
//! here, and the remaining ones are listed in `KNOWN_DEVIATIONS.md`.
//!
//! libstdc++ matches `char`s, that is bytes: a multi-byte UTF-8 character
//! is a sequence of bytes, both in the pattern and in bracket expressions,
//! where each byte is a member of its own. The translation therefore
//! writes every byte above ASCII as `\xNN` and matches with Unicode off.

use crate::runtime::cell::OnceCell;
use std::ffi::c_char;
use std::fmt::Write as _;

use crate::error::{msg, ErrorKind, EvalError};

/// A pattern, translated once and compiled on first use for each of the
/// two ways it is matched.
pub struct Regex {
    /// The pattern as written, for error messages.
    ere: Vec<u8>,
    /// [`translate_ere`]'s result, without flags.
    body: String,
    search: OnceCell<regex::bytes::Regex>,
    whole: OnceCell<regex::bytes::Regex>,
}

impl Regex {
    /// Translates an ERE. Errors become Nix's "invalid regular expression".
    pub fn new(ere: &[u8]) -> Result<Regex, crate::Error> {
        let body = translate_ere(ere).ok_or_else(|| invalid(ere))?;
        Ok(Regex {
            ere: ere.to_vec(),
            body,
            search: OnceCell::new(),
            whole: OnceCell::new(),
        })
    }

    /// The pattern matching anywhere (`std::regex_search`, `split`).
    pub fn search(&self) -> Result<&regex::bytes::Regex, crate::Error> {
        self.compiled(&self.search, "", "")
    }

    /// The pattern matching only a whole string (`std::regex_match`,
    /// `match`).
    ///
    /// The `regex` crate's leftmost-first search, anchored at both ends,
    /// finds the same match (captures included) that a search of the
    /// unanchored pattern does whenever that one spans the whole string:
    /// both try the pattern's alternatives at position 0 in the same order
    /// of preference, and the first success of the unanchored search, if
    /// it spans the string, is also the first one the anchors accept. When
    /// the unanchored search finds something shorter, only the anchored
    /// one can tell whether a whole-string match exists. So this one search
    /// is all `match` needs.
    pub fn whole(&self) -> Result<&regex::bytes::Regex, crate::Error> {
        self.compiled(&self.whole, "^(?:", ")$")
    }

    /// `body` wrapped in `before` and `after`, compiled once. Wrapping a
    /// valid body in a group keeps it valid and an invalid one invalid:
    /// [`translate_ere`] rejects unbalanced parentheses and `(?` (which
    /// could open `regex`'s own group syntax).
    fn compiled<'s>(
        &'s self,
        cell: &'s OnceCell<regex::bytes::Regex>,
        before: &str,
        after: &str,
    ) -> Result<&'s regex::bytes::Regex, crate::Error> {
        if let Some(r) = cell.get() {
            return Ok(r);
        }
        let pattern = format!("(?s-u){before}{}{after}", self.body);
        let r = regex::bytes::RegexBuilder::new(&pattern)
            .size_limit(64 << 20)
            .build()
            .map_err(|_| invalid(&self.ere))?;
        Ok(cell.get_or_init(|| r))
    }
}

fn invalid(ere: &[u8]) -> crate::Error {
    EvalError::new(
        ErrorKind::Eval,
        msg!("invalid regular expression '", ere, "'"),
    )
}

/// What `\` may escape in an ERE: libstdc++, built strictly (as Nix is),
/// rejects escaping any other character.
const ESCAPABLE: &[u8] = b".[\\()*+?{|^$";

/// Rewrites a POSIX ERE into `regex` syntax over bytes (the caller adds
/// the `(?s-u)` flags), or `None` for a pattern libstdc++ rejects:
/// * a byte above ASCII is written `\xNN`, so that each byte is one
///   character (a repetition after `é` repeats its last byte, as in Nix);
/// * `\` escapes only [`ESCAPABLE`]; `{` must open an interval;
/// * parentheses must balance and `(` must not be followed by `?`;
/// * bracket expressions are rewritten member by member
///   ([`translate_bracket`]);
/// * a repetition (`*`, `+`, `?`, `{…}`) needs something to repeat: an
///   atom, or a repetition, which it repeats again. libstdc++ applies
///   repetitions to an atom one after the other (`_M_term`), greedily in
///   ERE, so `a*?` is `(a*)?` and `a{2}{3}` is `(a{2}){3}`; `regex` would
///   read `a*?` as a lazy `a*`, so a repetition of a repetition repeats a
///   group of its own, `(?:a*)?`. At the start, after `(` or `|` and
///   after the assertions `^` and `$` there is nothing to repeat
///   (`error_badrepeat`).
fn translate_ere(ere: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(ere.len() + 8);
    // Where each open group starts in `out`.
    let mut groups: Vec<usize> = Vec::new();
    // Where what a repetition here would repeat starts in `out`: the last
    // atom, with the repetitions already applied to it.
    let mut operand: Option<usize> = None;
    // Whether `out` ends with a repetition of `operand`.
    let mut repeated = false;
    let mut i = 0;
    while i < ere.len() {
        let c = ere[i];
        let start = out.len();
        i += 1;
        let repetition = match c {
            b'*' | b'+' | b'?' => &ere[i - 1..i],
            b'{' => {
                // `{n}`, `{n,}` or `{n,m}`.
                let end = i + ere[i..].iter().position(|&c| c == b'}')?;
                let (min, max) = match ere[i..end].iter().position(|&c| c == b',') {
                    Some(comma) => (&ere[i..i + comma], Some(&ere[i + comma + 1..end])),
                    None => (&ere[i..end], None),
                };
                let digits = |d: &[u8]| d.iter().all(u8::is_ascii_digit);
                if min.is_empty() || !digits(min) || !max.is_none_or(digits) {
                    return None;
                }
                let interval = &ere[i - 1..=end];
                i = end + 1;
                interval
            }
            _ => {
                // An atom, or what leaves nothing to repeat.
                operand = Some(start);
                match c {
                    b'[' => translate_bracket(ere, &mut i, &mut out)?,
                    b'\\' => {
                        let e = *ere.get(i)?;
                        if !ESCAPABLE.contains(&e) {
                            return None;
                        }
                        out.push('\\');
                        out.push(e as char);
                        i += 1;
                    }
                    b'(' => {
                        if ere.get(i) == Some(&b'?') {
                            return None;
                        }
                        groups.push(start);
                        out.push('(');
                        operand = None;
                    }
                    b')' => {
                        operand = Some(groups.pop()?);
                        out.push(')');
                    }
                    b'|' | b'^' | b'$' => {
                        out.push(c as char);
                        operand = None;
                    }
                    0x80.. => push_byte(&mut out, c),
                    _ => out.push(c as char),
                }
                repeated = false;
                continue;
            }
        };
        let from = operand?;
        if repeated {
            out.insert_str(from, "(?:");
            out.push(')');
        }
        out.push_str(std::str::from_utf8(repetition).expect("repetitions are ASCII"));
        repeated = true;
    }
    groups.is_empty().then_some(out)
}

/// A byte as a `regex` literal that means that byte in any position,
/// bracket expressions included.
fn push_byte(out: &mut String, b: u8) {
    write!(out, "\\x{b:02X}").expect("writing to a String cannot fail");
}

/// What a bracket expression's term was, for the rules about ranges
/// (libstdc++'s `_BracketState`).
#[derive(Clone, Copy)]
enum Last {
    Nothing,
    Char(u8),
    Class,
}

/// A token inside a bracket expression (libstdc++'s `_M_scan_in_bracket`).
enum Token<'e> {
    End,
    Dash,
    Char(u8),
    Collating(&'e [u8]),
    Equivalence(&'e [u8]),
    Class(&'e [u8]),
}

/// The next token of the bracket expression at `ere[*i..]`, `None` at the
/// end of the pattern. A `]` right after `[` or `[^` is a character.
fn bracket_token<'e>(ere: &'e [u8], i: &mut usize, at_start: bool) -> Option<Token<'e>> {
    let c = *ere.get(*i)?;
    *i += 1;
    Some(match c {
        b'-' => Token::Dash,
        b']' if !at_start => Token::End,
        b'[' => match *ere.get(*i)? {
            kind @ (b'.' | b'=' | b':') => {
                // `[.name.]`, `[=name=]`, `[:name:]`: up to the first
                // `kind`, which `]` must follow.
                let start = *i + 1;
                let len = ere[start..].iter().position(|&c| c == kind)?;
                if ere.get(start + len + 1) != Some(&b']') {
                    return None;
                }
                *i = start + len + 2;
                let name = &ere[start..start + len];
                match kind {
                    b'.' => Token::Collating(name),
                    b'=' => Token::Equivalence(name),
                    _ => Token::Class(name),
                }
            }
            _ => Token::Char(b'['),
        },
        _ => Token::Char(c),
    })
}

/// Rewrites the bracket expression after the `[` at `ere[*i - 1]` the way
/// libstdc++'s `_M_insert_bracket_matcher` and `_M_expression_term` read
/// it: `]` first is a member, `-` is a member first or last, a range runs
/// between two single characters compared as C `char`s, a character class
/// cannot start a range. Members are written as `\xNN` and ranges as
/// `\xNN-\xNN`, which mean the same in `regex`'s byte classes whatever
/// the byte.
fn translate_bracket(ere: &[u8], i: &mut usize, out: &mut String) -> Option<()> {
    out.push('[');
    if ere.get(*i) == Some(&b'^') {
        out.push('^');
        *i += 1;
    }
    // A single character is held back in `last` until it is known not to
    // start a range (libstdc++'s `__push_char` and `__push_class`).
    let push_char = |out: &mut String, last: &mut Last, c: u8| {
        if let Last::Char(l) = *last {
            push_byte(out, l);
        }
        *last = Last::Char(c);
    };
    let push_class = |out: &mut String, last: &mut Last| {
        if let Last::Char(l) = *last {
            push_byte(out, l);
        }
        *last = Last::Class;
    };
    let mut last = Last::Nothing;
    let mut token = bracket_token(ere, i, true)?;
    match token {
        Token::Char(c) => {
            last = Last::Char(c);
            token = bracket_token(ere, i, false)?;
        }
        Token::Dash => {
            last = Last::Char(b'-');
            token = bracket_token(ere, i, false)?;
        }
        _ => {}
    }
    loop {
        match token {
            Token::End => break,
            Token::Char(c) => push_char(out, &mut last, c),
            Token::Collating(name) => push_char(out, &mut last, collating_element(name)?),
            Token::Equivalence(name) => {
                push_class(out, &mut last);
                let c = collating_element(name)?;
                // `transform_primary` lowercases: in the C locale a
                // letter's class is both its cases.
                push_byte(out, c.to_ascii_lowercase());
                push_byte(out, c.to_ascii_uppercase());
            }
            Token::Class(name) => {
                push_class(out, &mut last);
                out.push_str("[:");
                out.push_str(character_class(name)?);
                out.push_str(":]");
            }
            Token::Dash => {
                token = bracket_token(ere, i, false)?;
                match (last, token) {
                    // `-` before the closing `]` is a member.
                    (_, Token::End) => {
                        push_char(out, &mut last, b'-');
                        break;
                    }
                    (Last::Char(l), Token::Char(r)) => push_range(out, l, r)?,
                    (Last::Char(l), Token::Dash) => push_range(out, l, b'-')?,
                    _ => return None,
                }
                last = Last::Nothing;
            }
        }
        token = bracket_token(ere, i, false)?;
    }
    if let Last::Char(l) = last {
        push_byte(out, l);
    }
    out.push(']');
    Some(())
}

/// `_M_make_range`: the bytes from `l` to `r` as libstdc++ compares them,
/// as C `char`s. Where `char` is signed (x86-64) a range from a byte
/// above ASCII to one below wraps around: `[é-z]` holds `\xA9`–`\xFF`
/// and `\x00`–`z`.
fn push_range(out: &mut String, l: u8, r: u8) -> Option<()> {
    let (lc, rc) = (l as c_char, r as c_char);
    if lc > rc {
        return None;
    }
    let mut range = |from: u8, to: u8| {
        push_byte(out, from);
        out.push('-');
        push_byte(out, to);
    };
    if i32::from(lc) < 0 && i32::from(rc) >= 0 {
        range(0x00, r);
        range(l, 0xFF);
    } else {
        range(l, r);
    }
    Some(())
}

/// `regex_traits::lookup_collatename` (libstdc++): the character named by
/// a `[.name.]` or `[=name=]`, by its POSIX name (`space`, `hyphen`, …)
/// or, for a letter, itself.
fn collating_element(name: &[u8]) -> Option<u8> {
    #[rustfmt::skip]
    const NAMES: [&str; 128] = [
        "NUL", "SOH", "STX", "ETX", "EOT", "ENQ", "ACK", "alert", "backspace", "tab", "newline",
        "vertical-tab", "form-feed", "carriage-return", "SO", "SI", "DLE", "DC1", "DC2", "DC3",
        "DC4", "NAK", "SYN", "ETB", "CAN", "EM", "SUB", "ESC", "IS4", "IS3", "IS2", "IS1",
        "space", "exclamation-mark", "quotation-mark", "number-sign", "dollar-sign",
        "percent-sign", "ampersand", "apostrophe", "left-parenthesis", "right-parenthesis",
        "asterisk", "plus-sign", "comma", "hyphen", "period", "slash", "zero", "one", "two",
        "three", "four", "five", "six", "seven", "eight", "nine", "colon", "semicolon",
        "less-than-sign", "equals-sign", "greater-than-sign", "question-mark", "commercial-at",
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
        "R", "S", "T", "U", "V", "W", "X", "Y", "Z", "left-square-bracket", "backslash",
        "right-square-bracket", "circumflex", "underscore", "grave-accent", "a", "b", "c", "d",
        "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u",
        "v", "w", "x", "y", "z", "left-curly-bracket", "vertical-line", "right-curly-bracket",
        "tilde", "DEL",
    ];
    let c = NAMES.iter().position(|n| n.as_bytes() == name)?;
    Some(u8::try_from(c).expect("the table has 128 entries"))
}

/// `regex_traits::lookup_classname` (libstdc++), case-insensitive, as
/// the `regex` crate names the class.
fn character_class(name: &[u8]) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_slice() {
        b"d" | b"digit" => "digit",
        b"w" => "word",
        b"s" | b"space" => "space",
        b"alnum" => "alnum",
        b"alpha" => "alpha",
        b"blank" => "blank",
        b"cntrl" => "cntrl",
        b"graph" => "graph",
        b"lower" => "lower",
        b"print" => "print",
        b"punct" => "punct",
        b"upper" => "upper",
        b"xdigit" => "xdigit",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_eres() {
        assert_eq!(translate_ere(br"[\]a").unwrap(), r"[\x5C]a");
        assert_eq!(translate_ere(b"[[:alpha:]]+").unwrap(), "[[:alpha:]]+");
        assert_eq!(translate_ere(b"[^]a-c]").unwrap(), r"[^\x5D\x61-\x63]");
        assert_eq!(translate_ere(b"a{2}").unwrap(), "a{2}");
        assert_eq!(translate_ere(br"\.").unwrap(), r"\.");
        assert_eq!(translate_ere("é+".as_bytes()).unwrap(), r"\xC3\xA9+");
        assert_eq!(translate_ere(b"[\xff]").unwrap(), r"[\xFF]");
        assert_eq!(
            translate_ere(b"[[.space.][=a=]]").unwrap(),
            r"[\x20\x61\x41]"
        );
        // A repetition of a repetition repeats a group.
        assert_eq!(translate_ere(b"a*?").unwrap(), "(?:a*)?");
        assert_eq!(translate_ere(b"(a)+?b").unwrap(), "(?:(a)+)?b");
        assert_eq!(translate_ere(b"a{2}{3}*").unwrap(), "(?:(?:a{2}){3})*");
        assert_eq!(translate_ere(b"[ab]+?").unwrap(), r"(?:[\x61\x62]+)?");
        assert_eq!(translate_ere("é*?".as_bytes()).unwrap(), r"\xC3(?:\xA9*)?");
        assert_eq!(translate_ere(b"a|(b)c*").unwrap(), "a|(b)c*");
        for rejected in [
            &br"\d"[..],
            b"a{x}",
            b"a{,2}",
            b"(?:a)",
            b"a)(b",
            b"(a",
            b"[a",
            b"[^]",
            b"[z-a]",
            b"[a-c-e]",
            b"[[:alpha:]-z]",
            b"[[:word:]]",
            b"[[.0.]]",
            b"[a-[.c.]]",
            b"^*a",
            b"a$+",
            b"(*a)",
            b"a|*b",
            b"*a",
            b"{2}",
        ] {
            assert_eq!(
                translate_ere(rejected),
                None,
                "{}",
                String::from_utf8_lossy(rejected)
            );
        }
    }

    #[test]
    fn ranges_compare_c_chars() {
        let mut out = String::new();
        push_range(&mut out, b'a', b'z').unwrap();
        assert_eq!(out, r"\x61-\x7A");
        let mut out = String::new();
        let wrapped = push_range(&mut out, 0xA9, b'z');
        if c_char::MIN == 0 {
            assert_eq!(wrapped, None);
        } else {
            assert_eq!(out, r"\x00-\x7A\xA9-\xFF");
        }
    }

    #[test]
    fn matches() {
        let r = Regex::new(b"a(b)(c)").unwrap();
        let caps = r.search().unwrap().captures(b"xabc").unwrap();
        assert_eq!(&caps[1], b"b");
        assert!(r.whole().unwrap().captures(b"xabc").is_none());
        assert!(Regex::new(b"(").is_err());
    }
}
