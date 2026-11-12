//! nlohmann::json's parser as Nix uses it (`json::parse`,
//! `json::sax_parse`; nlohmann_json 3.12.0, without comments): what it
//! accepts, the events it reports to a SAX handler (`json_sax`), and its
//! errors, byte for byte.
//!
//! Nix parses JSON documents of the outside world (`builtins.fromJSON`,
//! a Git input's `publicKeys`, an LFS server's answers) with it, and its
//! exceptions are what Nix prints when they do not parse:
//! `[json.exception.parse_error.101] parse error at line 1, column 8:
//! syntax error while parsing value - invalid literal; last read: '1,
//! tru]'`. The lexer and the parser here follow nlohmann's
//! (`detail/input/lexer.hpp`, `detail/input/parser.hpp`) step by step,
//! since the line, the column and the "last read" text depend on exactly
//! which bytes it has read when it gives up:
//!
//! - a position counts bytes, and counts reading past the end too;
//! - "last read" is everything read since the start of the last string
//!   or number token (whitespace, punctuation and literals included),
//!   control characters shown as `<U+000A>`;
//! - a byte order mark is skipped, a NUL byte outside a string ends the
//!   input, and strings must be valid UTF-8;
//! - integers that do not fit in 64 bits are read as doubles, and a
//!   double that overflows is an error of its own
//!   (`[json.exception.out_of_range.406] number overflow parsing '1e400'`).
//!
//! [`sax_parse`] reports each value as it reads it, so a handler's own
//! error (Nix's "unsigned json number … outside of Nix integer range")
//! comes in document order with the syntax errors, as in Nix.
//! [`nlohmann_parse_error`] only checks a document.

use std::fmt;

/// An exception nlohmann's parser throws: its `what()`. Bytes, as "last
/// read" quotes the input as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    message: Vec<u8>,
}

impl ParseError {
    /// The message, as nlohmann writes it.
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// The message as text, bytes that are not UTF-8 replaced.
    pub fn message_lossy(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.message))
    }
}

impl std::error::Error for ParseError {}

/// The events of `json_sax`, as [`sax_parse`] reports them while it reads
/// a document. A handler that returns an error stops the parse, as a SAX
/// handler's exception does in Nix. Each method does nothing by default.
pub trait Sax {
    type Error;

    fn null(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn boolean(&mut self, value: bool) -> Result<(), Self::Error> {
        let _ = value;
        Ok(())
    }

    /// A negative integer that fits in 64 bits (`-0` included).
    fn number_integer(&mut self, value: i64) -> Result<(), Self::Error> {
        let _ = value;
        Ok(())
    }

    /// A non-negative integer that fits in 64 bits, unsigned.
    fn number_unsigned(&mut self, value: u64) -> Result<(), Self::Error> {
        let _ = value;
        Ok(())
    }

    /// A number with a fraction or an exponent, or an integer too large
    /// for 64 bits; always finite. `text` is the number as written.
    fn number_float(&mut self, value: f64, text: &[u8]) -> Result<(), Self::Error> {
        let _ = (value, text);
        Ok(())
    }

    /// A string value, unescaped: valid UTF-8, which may hold NUL bytes
    /// (`\u0000`).
    fn string(&mut self, value: &[u8]) -> Result<(), Self::Error> {
        let _ = value;
        Ok(())
    }

    fn start_object(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// An object's key, unescaped like a string, before its value.
    fn key(&mut self, key: &[u8]) -> Result<(), Self::Error> {
        let _ = key;
        Ok(())
    }

    fn end_object(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn start_array(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn end_array(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Why [`sax_parse`] stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SaxError<E> {
    /// The document does not parse (nlohmann's `parse_error` or
    /// `out_of_range` exception, as the handler's `parse_error` gets it).
    Parse(ParseError),
    /// The handler refused a value.
    Handler(E),
}

/// `json::sax_parse(input, sax)` (strict: nothing but whitespace may
/// follow the value): reads `input`, reporting each value to `sax` in
/// document order.
pub fn sax_parse<S: Sax>(input: &[u8], sax: &mut S) -> Result<(), SaxError<S::Error>> {
    let mut parser = Parser {
        lexer: Lexer::new(input),
        last_token: Token::Uninitialized,
    };
    parser.get_token();
    parser.parse_value(sax)?;
    if parser.get_token() != Token::EndOfInput {
        return Err(SaxError::Parse(
            parser.syntax_error(Token::EndOfInput, "value"),
        ));
    }
    Ok(())
}

/// The error nlohmann's `json::parse(input)` throws for `input`, or `None`
/// when it parses. Bytes of the input it quotes that are not UTF-8 are
/// replaced (see [`sax_parse`] for the bytes).
pub fn nlohmann_parse_error(input: &[u8]) -> Option<String> {
    match sax_parse(input, &mut Check) {
        Ok(()) => None,
        Err(SaxError::Parse(e)) => Some(e.message_lossy()),
        Err(SaxError::Handler(never)) => match never {},
    }
}

/// A handler that takes every value: `json::accept`, with the errors.
struct Check;

impl Sax for Check {
    type Error = std::convert::Infallible;
}

/// `lexer_base::token_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Uninitialized,
    LiteralTrue,
    LiteralFalse,
    LiteralNull,
    ValueString,
    ValueUnsigned,
    ValueInteger,
    ValueFloat,
    BeginArray,
    BeginObject,
    EndArray,
    EndObject,
    NameSeparator,
    ValueSeparator,
    ParseError,
    EndOfInput,
    LiteralOrValue,
}

impl Token {
    /// `token_type_name`.
    fn name(self) -> &'static str {
        match self {
            Token::Uninitialized => "<uninitialized>",
            Token::LiteralTrue => "true literal",
            Token::LiteralFalse => "false literal",
            Token::LiteralNull => "null literal",
            Token::ValueString => "string literal",
            Token::ValueUnsigned | Token::ValueInteger | Token::ValueFloat => "number literal",
            Token::BeginArray => "'['",
            Token::BeginObject => "'{'",
            Token::EndArray => "']'",
            Token::EndObject => "'}'",
            Token::NameSeparator => "':'",
            Token::ValueSeparator => "','",
            Token::ParseError => "<parse error>",
            Token::EndOfInput => "end of input",
            Token::LiteralOrValue => "'[', '{', or a literal",
        }
    }
}

/// `position_t`: where the lexer is, counting bytes read (and reads past
/// the end).
#[derive(Clone, Copy, Debug, Default)]
struct Position {
    chars_read_total: usize,
    chars_read_current_line: usize,
    lines_read: usize,
}

/// `detail::lexer` over a byte buffer (`iterator_input_adapter`).
struct Lexer<'i> {
    input: &'i [u8],
    /// The index of the next byte `get` takes from the input.
    next: usize,
    /// The last byte read; `None` past the end (`eof()`).
    current: Option<u8>,
    /// `get` gives `current` again instead of reading on.
    next_unget: bool,
    position: Position,
    /// Where `token_string` (every byte read since the last `reset`, for
    /// messages) starts in the input; it ends at the last byte read and
    /// not given back.
    token_start: usize,
    /// The unescaped string, or the number's text.
    token_buffer: Vec<u8>,
    error_message: &'static str,
    value_integer: i64,
    value_unsigned: u64,
    value_float: f64,
}

impl<'i> Lexer<'i> {
    fn new(input: &'i [u8]) -> Lexer<'i> {
        Lexer {
            input,
            next: 0,
            current: None,
            next_unget: false,
            position: Position::default(),
            token_start: 0,
            token_buffer: Vec::new(),
            error_message: "",
            value_integer: 0,
            value_unsigned: 0,
            value_float: 0.0,
        }
    }

    /// `get`: the next byte (or the last one again after `unget`).
    fn get(&mut self) -> Option<u8> {
        self.position.chars_read_total += 1;
        self.position.chars_read_current_line += 1;
        if self.next_unget {
            self.next_unget = false;
        } else {
            self.current = self.input.get(self.next).copied();
            if self.current.is_some() {
                self.next += 1;
            }
        }
        if self.current == Some(b'\n') {
            self.position.lines_read += 1;
            self.position.chars_read_current_line = 0;
        }
        self.current
    }

    /// `unget`: give the last byte back.
    fn unget(&mut self) {
        self.next_unget = true;
        self.position.chars_read_total -= 1;
        if self.position.chars_read_current_line == 0 {
            if self.position.lines_read > 0 {
                self.position.lines_read -= 1;
            }
        } else {
            self.position.chars_read_current_line -= 1;
        }
    }

    /// The end of `token_string`: the bytes read and not given back.
    fn consumed(&self) -> usize {
        if self.next_unget && self.current.is_some() {
            self.next - 1
        } else {
            self.next
        }
    }

    /// `reset`: a string or number token starts at `current`.
    fn reset(&mut self) {
        self.token_buffer.clear();
        self.token_start = self.consumed() - 1;
    }

    fn add(&mut self, c: u8) {
        self.token_buffer.push(c);
    }

    /// `get_token_string`: what was read since the token started, control
    /// characters as `<U+XXXX>`.
    fn token_string(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for &c in &self.input[self.token_start..self.consumed()] {
            if c <= 0x1F {
                out.extend_from_slice(format!("<U+{c:04X}>").as_bytes());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// `skip_bom`: whether the input does not start with a broken byte
    /// order mark (a whole one is skipped).
    fn skip_bom(&mut self) -> bool {
        if self.get() == Some(0xEF) {
            return self.get() == Some(0xBB) && self.get() == Some(0xBF);
        }
        self.unget();
        true
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.get(), Some(b' ' | b'\t' | b'\n' | b'\r')) {}
    }

    /// `scan`: the next token.
    fn scan(&mut self) -> Token {
        if self.position.chars_read_total == 0 && !self.skip_bom() {
            self.error_message = "invalid BOM; must be 0xEF 0xBB 0xBF if given";
            return Token::ParseError;
        }
        self.skip_whitespace();
        match self.current {
            Some(b'[') => Token::BeginArray,
            Some(b']') => Token::EndArray,
            Some(b'{') => Token::BeginObject,
            Some(b'}') => Token::EndObject,
            Some(b':') => Token::NameSeparator,
            Some(b',') => Token::ValueSeparator,
            Some(b't') => self.scan_literal(b"true", Token::LiteralTrue),
            Some(b'f') => self.scan_literal(b"false", Token::LiteralFalse),
            Some(b'n') => self.scan_literal(b"null", Token::LiteralNull),
            Some(b'"') => self.scan_string(),
            Some(b'-' | b'0'..=b'9') => self.scan_number(),
            // The NUL byte ends the input, as at the end of a C string.
            Some(0) | None => Token::EndOfInput,
            Some(_) => {
                self.error_message = "invalid literal";
                Token::ParseError
            }
        }
    }

    /// `scan_literal`: the rest of `literal`, whose first byte was read.
    fn scan_literal(&mut self, literal: &[u8], token: Token) -> Token {
        for &expected in &literal[1..] {
            if self.get() != Some(expected) {
                self.error_message = "invalid literal";
                return Token::ParseError;
            }
        }
        token
    }

    /// `get_codepoint`: the four hex digits after `\u`, or `None` at the
    /// first byte that is not one.
    fn get_codepoint(&mut self) -> Option<u32> {
        let mut codepoint = 0;
        for _ in 0..4 {
            let digit = match self.get() {
                Some(c @ b'0'..=b'9') => c - b'0',
                Some(c @ b'A'..=b'F') => c - b'A' + 10,
                Some(c @ b'a'..=b'f') => c - b'a' + 10,
                _ => return None,
            };
            codepoint = (codepoint << 4) | u32::from(digit);
        }
        Some(codepoint)
    }

    /// `next_byte_in_range`: `current` starts a UTF-8 sequence whose
    /// further bytes must lie in `ranges`.
    fn next_byte_in_range(&mut self, ranges: &[(u8, u8)]) -> bool {
        let lead = self.current.expect("a sequence starts with the byte read");
        self.add(lead);
        for &(low, high) in ranges {
            match self.get() {
                Some(c) if (low..=high).contains(&c) => self.add(c),
                _ => {
                    self.error_message = "invalid string: ill-formed UTF-8 byte";
                    return false;
                }
            }
        }
        true
    }

    /// `scan_string`: a string after its opening quote.
    fn scan_string(&mut self) -> Token {
        self.reset();
        loop {
            // A run of plain ASCII is what `get` and `add` would make of it
            // one byte at a time (it holds no newline).
            if !self.next_unget {
                let run = self.input[self.next..]
                    .iter()
                    .take_while(|&&c| (0x20..=0x7F).contains(&c) && c != b'"' && c != b'\\')
                    .count();
                if run > 0 {
                    let bytes = &self.input[self.next..self.next + run];
                    self.token_buffer.extend_from_slice(bytes);
                    self.current = Some(bytes[run - 1]);
                    self.next += run;
                    self.position.chars_read_total += run;
                    self.position.chars_read_current_line += run;
                }
            }
            match self.get() {
                None => {
                    self.error_message = "invalid string: missing closing quote";
                    return Token::ParseError;
                }
                Some(b'"') => return Token::ValueString,
                Some(b'\\') => {
                    if !self.scan_escape() {
                        return Token::ParseError;
                    }
                }
                Some(c @ 0x00..=0x1F) => {
                    self.error_message = CONTROL_CHARACTER_ERRORS[usize::from(c)];
                    return Token::ParseError;
                }
                Some(c @ 0x20..=0x7F) => self.add(c),
                Some(0xC2..=0xDF) => {
                    if !self.next_byte_in_range(&[(0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xE0) => {
                    if !self.next_byte_in_range(&[(0xA0, 0xBF), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xE1..=0xEC | 0xEE | 0xEF) => {
                    if !self.next_byte_in_range(&[(0x80, 0xBF), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xED) => {
                    if !self.next_byte_in_range(&[(0x80, 0x9F), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xF0) => {
                    if !self.next_byte_in_range(&[(0x90, 0xBF), (0x80, 0xBF), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xF1..=0xF3) => {
                    if !self.next_byte_in_range(&[(0x80, 0xBF), (0x80, 0xBF), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(0xF4) => {
                    if !self.next_byte_in_range(&[(0x80, 0x8F), (0x80, 0xBF), (0x80, 0xBF)]) {
                        return Token::ParseError;
                    }
                }
                Some(_) => {
                    self.error_message = "invalid string: ill-formed UTF-8 byte";
                    return Token::ParseError;
                }
            }
        }
    }

    /// The escape after a backslash in a string; `false` (with the
    /// message set) when it is not one.
    fn scan_escape(&mut self) -> bool {
        let c = match self.get() {
            Some(b'"') => b'"',
            Some(b'\\') => b'\\',
            Some(b'/') => b'/',
            Some(b'b') => 0x08,
            Some(b'f') => 0x0C,
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(b't') => b'\t',
            Some(b'u') => return self.scan_unicode_escape(),
            _ => {
                self.error_message = "invalid string: forbidden character after backslash";
                return false;
            }
        };
        self.add(c);
        true
    }

    /// `\uXXXX`, or a surrogate pair of them, as UTF-8.
    fn scan_unicode_escape(&mut self) -> bool {
        const HEX: &str = "invalid string: '\\u' must be followed by 4 hex digits";
        const HIGH: &str =
            "invalid string: surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF";
        let Some(first) = self.get_codepoint() else {
            self.error_message = HEX;
            return false;
        };
        let codepoint = if (0xD800..=0xDBFF).contains(&first) {
            // Only reads `u` after a backslash.
            if !(self.get() == Some(b'\\') && self.get() == Some(b'u')) {
                self.error_message = HIGH;
                return false;
            }
            let Some(second) = self.get_codepoint() else {
                self.error_message = HEX;
                return false;
            };
            if !(0xDC00..=0xDFFF).contains(&second) {
                self.error_message = HIGH;
                return false;
            }
            (first << 10) + second - 0x35F_DC00
        } else if (0xDC00..=0xDFFF).contains(&first) {
            self.error_message =
                "invalid string: surrogate U+DC00..U+DFFF must follow U+D800..U+DBFF";
            return false;
        } else {
            first
        };
        let c = char::from_u32(codepoint).expect("surrogates are paired or refused");
        let mut utf8 = [0; 4];
        self.token_buffer
            .extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        true
    }

    /// `scan_number`: the state machine of RFC 8259's number grammar, then
    /// the value: an unsigned or signed 64-bit integer when it is one and
    /// fits (`strtoull`/`strtoll`), else a double (`strtod`).
    fn scan_number(&mut self) -> Token {
        #[derive(Clone, Copy)]
        enum State {
            Minus,
            Zero,
            Any1,
            Decimal1,
            Decimal2,
            Exponent,
            Sign,
            Any2,
            Done,
        }
        self.reset();
        let first = self.current.expect("a number starts with the byte read");
        self.add(first);
        let mut number_type = Token::ValueUnsigned;
        let mut state = match first {
            b'-' => State::Minus,
            b'0' => State::Zero,
            _ => State::Any1,
        };
        loop {
            state = match state {
                State::Minus => {
                    number_type = Token::ValueInteger;
                    match self.get() {
                        Some(c @ b'0') => {
                            self.add(c);
                            State::Zero
                        }
                        Some(c @ b'1'..=b'9') => {
                            self.add(c);
                            State::Any1
                        }
                        _ => {
                            self.error_message = "invalid number; expected digit after '-'";
                            return Token::ParseError;
                        }
                    }
                }
                State::Zero => match self.get() {
                    Some(c @ b'.') => {
                        self.add(c);
                        State::Decimal1
                    }
                    Some(c @ (b'e' | b'E')) => {
                        self.add(c);
                        State::Exponent
                    }
                    _ => State::Done,
                },
                State::Any1 => match self.get() {
                    Some(c @ b'0'..=b'9') => {
                        self.add(c);
                        State::Any1
                    }
                    Some(c @ b'.') => {
                        self.add(c);
                        State::Decimal1
                    }
                    Some(c @ (b'e' | b'E')) => {
                        self.add(c);
                        State::Exponent
                    }
                    _ => State::Done,
                },
                State::Decimal1 => {
                    number_type = Token::ValueFloat;
                    match self.get() {
                        Some(c @ b'0'..=b'9') => {
                            self.add(c);
                            State::Decimal2
                        }
                        _ => {
                            self.error_message = "invalid number; expected digit after '.'";
                            return Token::ParseError;
                        }
                    }
                }
                State::Decimal2 => match self.get() {
                    Some(c @ b'0'..=b'9') => {
                        self.add(c);
                        State::Decimal2
                    }
                    Some(c @ (b'e' | b'E')) => {
                        self.add(c);
                        State::Exponent
                    }
                    _ => State::Done,
                },
                State::Exponent => {
                    number_type = Token::ValueFloat;
                    match self.get() {
                        Some(c @ (b'+' | b'-')) => {
                            self.add(c);
                            State::Sign
                        }
                        Some(c @ b'0'..=b'9') => {
                            self.add(c);
                            State::Any2
                        }
                        _ => {
                            self.error_message =
                                "invalid number; expected '+', '-', or digit after exponent";
                            return Token::ParseError;
                        }
                    }
                }
                State::Sign => match self.get() {
                    Some(c @ b'0'..=b'9') => {
                        self.add(c);
                        State::Any2
                    }
                    _ => {
                        self.error_message = "invalid number; expected digit after exponent sign";
                        return Token::ParseError;
                    }
                },
                State::Any2 => match self.get() {
                    Some(c @ b'0'..=b'9') => {
                        self.add(c);
                        State::Any2
                    }
                    _ => State::Done,
                },
                State::Done => break,
            };
        }
        self.unget();
        let text = std::str::from_utf8(&self.token_buffer).expect("a number is ASCII");
        match number_type {
            Token::ValueUnsigned => {
                if let Ok(value) = text.parse() {
                    self.value_unsigned = value;
                    return Token::ValueUnsigned;
                }
            }
            Token::ValueInteger => {
                if let Ok(value) = text.parse() {
                    self.value_integer = value;
                    return Token::ValueInteger;
                }
            }
            _ => {}
        }
        // Rust's parse rounds correctly, as `strtod` does; too large a
        // number is infinite, which the parser refuses.
        self.value_float = text.parse().expect("the grammar's numbers are floats");
        Token::ValueFloat
    }
}

/// The lexer's message for each control character in a string.
const CONTROL_CHARACTER_ERRORS: [&str; 32] = [
    "invalid string: control character U+0000 (NUL) must be escaped to \\u0000",
    "invalid string: control character U+0001 (SOH) must be escaped to \\u0001",
    "invalid string: control character U+0002 (STX) must be escaped to \\u0002",
    "invalid string: control character U+0003 (ETX) must be escaped to \\u0003",
    "invalid string: control character U+0004 (EOT) must be escaped to \\u0004",
    "invalid string: control character U+0005 (ENQ) must be escaped to \\u0005",
    "invalid string: control character U+0006 (ACK) must be escaped to \\u0006",
    "invalid string: control character U+0007 (BEL) must be escaped to \\u0007",
    "invalid string: control character U+0008 (BS) must be escaped to \\u0008 or \\b",
    "invalid string: control character U+0009 (HT) must be escaped to \\u0009 or \\t",
    "invalid string: control character U+000A (LF) must be escaped to \\u000A or \\n",
    "invalid string: control character U+000B (VT) must be escaped to \\u000B",
    "invalid string: control character U+000C (FF) must be escaped to \\u000C or \\f",
    "invalid string: control character U+000D (CR) must be escaped to \\u000D or \\r",
    "invalid string: control character U+000E (SO) must be escaped to \\u000E",
    "invalid string: control character U+000F (SI) must be escaped to \\u000F",
    "invalid string: control character U+0010 (DLE) must be escaped to \\u0010",
    "invalid string: control character U+0011 (DC1) must be escaped to \\u0011",
    "invalid string: control character U+0012 (DC2) must be escaped to \\u0012",
    "invalid string: control character U+0013 (DC3) must be escaped to \\u0013",
    "invalid string: control character U+0014 (DC4) must be escaped to \\u0014",
    "invalid string: control character U+0015 (NAK) must be escaped to \\u0015",
    "invalid string: control character U+0016 (SYN) must be escaped to \\u0016",
    "invalid string: control character U+0017 (ETB) must be escaped to \\u0017",
    "invalid string: control character U+0018 (CAN) must be escaped to \\u0018",
    "invalid string: control character U+0019 (EM) must be escaped to \\u0019",
    "invalid string: control character U+001A (SUB) must be escaped to \\u001A",
    "invalid string: control character U+001B (ESC) must be escaped to \\u001B",
    "invalid string: control character U+001C (FS) must be escaped to \\u001C",
    "invalid string: control character U+001D (GS) must be escaped to \\u001D",
    "invalid string: control character U+001E (RS) must be escaped to \\u001E",
    "invalid string: control character U+001F (US) must be escaped to \\u001F",
];

/// `detail::parser`.
struct Parser<'i> {
    lexer: Lexer<'i>,
    last_token: Token,
}

impl Parser<'_> {
    fn get_token(&mut self) -> Token {
        self.last_token = self.lexer.scan();
        self.last_token
    }

    /// `parse_error::create(101, position, what)`.
    fn parse_error(&self, what: &[u8]) -> ParseError {
        let position = self.lexer.position;
        let mut message = format!(
            "[json.exception.parse_error.101] parse error at line {}, column {}: ",
            position.lines_read + 1,
            position.chars_read_current_line
        )
        .into_bytes();
        message.extend_from_slice(what);
        ParseError { message }
    }

    /// `exception_message(expected, context)`, as a parse error.
    fn syntax_error(&self, expected: Token, context: &str) -> ParseError {
        let mut what = b"syntax error ".to_vec();
        if !context.is_empty() {
            what.extend_from_slice(format!("while parsing {context} ").as_bytes());
        }
        what.extend_from_slice(b"- ");
        if self.last_token == Token::ParseError {
            what.extend_from_slice(self.lexer.error_message.as_bytes());
            what.extend_from_slice(b"; last read: '");
            what.extend_from_slice(&self.lexer.token_string());
            what.push(b'\'');
        } else {
            what.extend_from_slice(format!("unexpected {}", self.last_token.name()).as_bytes());
        }
        if expected != Token::Uninitialized {
            what.extend_from_slice(format!("; expected {}", expected.name()).as_bytes());
        }
        self.parse_error(&what)
    }

    /// `sax_parse_internal`: one value, from the token just read, with
    /// the arrays and objects in it (iteratively, as nlohmann does, so
    /// nesting is not limited by the stack).
    fn parse_value<S: Sax>(&mut self, sax: &mut S) -> Result<(), SaxError<S::Error>> {
        let parse = SaxError::Parse;
        let handler = SaxError::Handler;
        // For each array or object being read: whether it is an array.
        let mut states: Vec<bool> = Vec::new();
        let mut skip_to_state_evaluation = false;
        loop {
            if skip_to_state_evaluation {
                skip_to_state_evaluation = false;
            } else {
                match self.last_token {
                    Token::BeginObject => {
                        sax.start_object().map_err(handler)?;
                        if self.get_token() == Token::EndObject {
                            sax.end_object().map_err(handler)?;
                        } else {
                            self.object_key(sax)?;
                            states.push(false);
                            self.get_token();
                            continue;
                        }
                    }
                    Token::BeginArray => {
                        sax.start_array().map_err(handler)?;
                        if self.get_token() == Token::EndArray {
                            sax.end_array().map_err(handler)?;
                        } else {
                            states.push(true);
                            continue;
                        }
                    }
                    Token::ValueFloat => {
                        let value = self.lexer.value_float;
                        if !value.is_finite() {
                            let mut message =
                                b"[json.exception.out_of_range.406] number overflow parsing '"
                                    .to_vec();
                            message.extend_from_slice(&self.lexer.token_string());
                            message.push(b'\'');
                            return Err(parse(ParseError { message }));
                        }
                        sax.number_float(value, &self.lexer.token_buffer)
                            .map_err(handler)?;
                    }
                    Token::LiteralFalse => sax.boolean(false).map_err(handler)?,
                    Token::LiteralNull => sax.null().map_err(handler)?,
                    Token::LiteralTrue => sax.boolean(true).map_err(handler)?,
                    Token::ValueInteger => sax
                        .number_integer(self.lexer.value_integer)
                        .map_err(handler)?,
                    Token::ValueString => sax.string(&self.lexer.token_buffer).map_err(handler)?,
                    Token::ValueUnsigned => sax
                        .number_unsigned(self.lexer.value_unsigned)
                        .map_err(handler)?,
                    Token::ParseError => {
                        return Err(parse(self.syntax_error(Token::Uninitialized, "value")))
                    }
                    Token::EndOfInput if self.lexer.position.chars_read_total == 1 => {
                        return Err(parse(self.parse_error(
                            b"attempting to parse an empty input; check that your input string or stream contains the expected JSON",
                        )))
                    }
                    _ => return Err(parse(self.syntax_error(Token::LiteralOrValue, "value"))),
                }
            }
            let Some(&in_array) = states.last() else {
                return Ok(());
            };
            if in_array {
                if self.get_token() == Token::ValueSeparator {
                    self.get_token();
                    continue;
                }
                if self.last_token == Token::EndArray {
                    sax.end_array().map_err(handler)?;
                    states.pop();
                    skip_to_state_evaluation = true;
                    continue;
                }
                return Err(parse(self.syntax_error(Token::EndArray, "array")));
            }
            if self.get_token() == Token::ValueSeparator {
                self.get_token();
                self.object_key(sax)?;
                self.get_token();
                continue;
            }
            if self.last_token == Token::EndObject {
                sax.end_object().map_err(handler)?;
                states.pop();
                skip_to_state_evaluation = true;
                continue;
            }
            return Err(parse(self.syntax_error(Token::EndObject, "object")));
        }
    }

    /// An object's key (the token just read) and the `:` after it.
    fn object_key<S: Sax>(&mut self, sax: &mut S) -> Result<(), SaxError<S::Error>> {
        if self.last_token != Token::ValueString {
            return Err(SaxError::Parse(
                self.syntax_error(Token::ValueString, "object key"),
            ));
        }
        sax.key(&self.lexer.token_buffer)
            .map_err(SaxError::Handler)?;
        if self.get_token() != Token::NameSeparator {
            return Err(SaxError::Parse(
                self.syntax_error(Token::NameSeparator, "object separator"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(input: &[u8]) -> String {
        nlohmann_parse_error(input).expect("the input does not parse")
    }

    /// Messages the reference `nix eval --expr 'builtins.fromJSON "…"'`
    /// prints for these documents (`tests/json.rs` compares many more
    /// with it directly).
    #[test]
    fn errors_are_nlohmanns() {
        let p = "[json.exception.parse_error.101] parse error at ";
        let cases: &[(&[u8], &str)] = &[
            (&b""[..], "line 1, column 1: attempting to parse an empty input; check that your input string or stream contains the expected JSON"),
            (b"   ", "line 1, column 4: syntax error while parsing value - unexpected end of input; expected '[', '{', or a literal"),
            (b"notjson", "line 1, column 2: syntax error while parsing value - invalid literal; last read: 'no'"),
            (b"[1, tru]", "line 1, column 8: syntax error while parsing value - invalid literal; last read: '1, tru]'"),
            (b"1 x", "line 1, column 3: syntax error while parsing value - invalid literal; last read: '1 x'; expected end of input"),
            (b"[1,]", "line 1, column 4: syntax error while parsing value - unexpected ']'; expected '[', '{', or a literal"),
            (b"{\"a\" 1}", "line 1, column 6: syntax error while parsing object separator - unexpected number literal; expected ':'"),
            (b"{\"a\": 1,}", "line 1, column 9: syntax error while parsing object key - unexpected '}'; expected string literal"),
            (b"[1 2]", "line 1, column 4: syntax error while parsing array - unexpected number literal; expected ']'"),
            (b"{\"a\": 1 \"b\"}", "line 1, column 11: syntax error while parsing object - unexpected string literal; expected '}'"),
            (b"\"abc", "line 1, column 5: syntax error while parsing value - invalid string: missing closing quote; last read: '\"abc'"),
            (b"\"\\uD800\\u0041\"", "line 1, column 13: syntax error while parsing value - invalid string: surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF; last read: '\"\\uD800\\u0041'"),
            (b"\"a\nb\"", "line 2, column 0: syntax error while parsing value - invalid string: control character U+000A (LF) must be escaped to \\u000A or \\n; last read: '\"a<U+000A>'"),
            (b"[\n1,\n  x]", "line 3, column 3: syntax error while parsing value - invalid literal; last read: '1,<U+000A>  x'"),
            (b"01", "line 1, column 2: syntax error while parsing value - unexpected number literal; expected end of input"),
            (b"1e+", "line 1, column 4: syntax error while parsing value - invalid number; expected digit after exponent sign; last read: '1e+'"),
            (b"nulll", "line 1, column 5: syntax error while parsing value - invalid literal; last read: 'nulll'; expected end of input"),
            (b"\"\xc3\xa9\" x", "line 1, column 6: syntax error while parsing value - invalid literal; last read: '\"\u{e9}\" x'; expected end of input"),
        ];
        for &(input, message) in cases {
            assert_eq!(error(input), format!("{p}{message}"), "{input:?}");
        }
        assert_eq!(
            error(b"[1, 1e400"),
            "[json.exception.out_of_range.406] number overflow parsing '1e400'"
        );
    }

    #[test]
    fn what_nlohmann_accepts_parses() {
        let inputs: [&[u8]; 6] = [
            &b"1"[..],
            b" [1, -0, 2.5e3, \"\\u00e9\\ud83d\\ude00\", true, false, null, {}] ",
            b"\xEF\xBB\xBF{\"a\": {\"b\": []}}",
            // A NUL byte ends the input.
            b"[1]\0garbage",
            b"18446744073709551616",
            b"-9223372036854775809",
        ];
        for input in inputs {
            assert_eq!(nlohmann_parse_error(input), None, "{input:?}");
        }
    }

    /// The events, in document order, numbers as nlohmann types them.
    #[test]
    fn events_come_in_document_order() {
        #[derive(Default)]
        struct Log(Vec<String>);
        impl Sax for Log {
            type Error = String;
            fn null(&mut self) -> Result<(), String> {
                self.0.push("null".into());
                Ok(())
            }
            fn number_integer(&mut self, v: i64) -> Result<(), String> {
                self.0.push(format!("i{v}"));
                Ok(())
            }
            fn number_unsigned(&mut self, v: u64) -> Result<(), String> {
                if v == 7 {
                    return Err("seven".into());
                }
                self.0.push(format!("u{v}"));
                Ok(())
            }
            fn number_float(&mut self, v: f64, text: &[u8]) -> Result<(), String> {
                self.0
                    .push(format!("f{v}:{}", String::from_utf8_lossy(text)));
                Ok(())
            }
            fn string(&mut self, v: &[u8]) -> Result<(), String> {
                self.0.push(format!("s{}", String::from_utf8_lossy(v)));
                Ok(())
            }
            fn key(&mut self, v: &[u8]) -> Result<(), String> {
                self.0.push(format!("k{}", String::from_utf8_lossy(v)));
                Ok(())
            }
            fn start_object(&mut self) -> Result<(), String> {
                self.0.push("{".into());
                Ok(())
            }
            fn end_object(&mut self) -> Result<(), String> {
                self.0.push("}".into());
                Ok(())
            }
            fn start_array(&mut self) -> Result<(), String> {
                self.0.push("[".into());
                Ok(())
            }
            fn end_array(&mut self) -> Result<(), String> {
                self.0.push("]".into());
                Ok(())
            }
        }
        let mut log = Log::default();
        sax_parse(
            br#"{"a": [1, -2, -0, 1.5, 1E2, 99999999999999999999, null], "b": {}, "c": "x\ty"}"#,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            log.0,
            [
                "{",
                "ka",
                "[",
                "u1",
                "i-2",
                "i0",
                "f1.5:1.5",
                "f100:1E2",
                "f100000000000000000000:99999999999999999999",
                "null",
                "]",
                "kb",
                "{",
                "}",
                "kc",
                "sx\ty",
                "}"
            ]
        );
        // A handler's error comes before a later syntax error.
        let mut log = Log::default();
        assert_eq!(
            sax_parse(b"[7, x", &mut log),
            Err(SaxError::Handler("seven".into()))
        );
        let mut log = Log::default();
        assert!(matches!(
            sax_parse(b"[x, 7", &mut log),
            Err(SaxError::Parse(_))
        ));
    }

    /// Nesting is not limited by the stack.
    #[test]
    fn deep_nesting_parses() {
        let depth = 1_000_000;
        let text = "[".repeat(depth) + &"]".repeat(depth);
        assert_eq!(nlohmann_parse_error(text.as_bytes()), None);
    }
}
