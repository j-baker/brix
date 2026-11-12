//! The lexer: a port of Nix's `lexer.l`.
//!
//! Flex lexers are a stack of start conditions; Nix uses them so that `${`
//! inside a string switches back to ordinary tokens until the matching `}`,
//! and so that a path literal continues after an interpolation. Each start
//! condition is a separate `logos` token enum here, and [`Lexer`] is the
//! driver that keeps the state stack, decides which enum to run at the
//! current offset, and turns the matches into [`Token`]s for the parser.
//!
//! Token boundaries are reproduced exactly, not only token kinds: Nix's
//! parser builds one AST node per string token, so where flex splits a
//! string into pieces is visible in `nix-instantiate --parse` output.

use logos::Logos;

use crate::ast::Pos;
use crate::error::ParseError;
use crate::parser::ParseOptions;

/// A token as the parser sees it. Byte slices borrow from the source;
/// strings own their (unescaped) text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok<'a> {
    Id(&'a [u8]),
    Int(i64),
    Float(f64),
    /// A piece of a `"…"` string (already unescaped), or a piece of a path
    /// after its first interpolation.
    Str(Vec<u8>),
    /// A piece of an indented string. `has_indentation` is false for the
    /// escape pieces (`''$`, `'''`, `''\x`, a lone `$` or `'`), which do not
    /// take part in the indentation computation.
    IndStr {
        text: Vec<u8>,
        has_indentation: bool,
    },
    /// The first piece of a path literal.
    Path(&'a [u8]),
    /// The first piece of a `~/…` path literal.
    HPath(&'a [u8]),
    /// `<nixpkgs>`, including the angle brackets.
    SPath(&'a [u8]),
    PathEnd,
    Uri(&'a [u8]),
    If,
    Then,
    Else,
    Assert,
    With,
    Let,
    In,
    Rec,
    Inherit,
    OrKw,
    Eq,
    NEq,
    Leq,
    Geq,
    Update,
    Concat,
    And,
    Or,
    Impl,
    PipeFrom,
    PipeInto,
    DollarCurly,
    IndStringOpen,
    IndStringClose,
    Ellipsis,
    Colon,
    At,
    Semi,
    Assign,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Dot,
    Question,
    Bang,
    Minus,
    Plus,
    Star,
    Slash,
    Lt,
    Gt,
    Comma,
    Quote,
    /// A byte that is not a token of the grammar (bison's `YYUNDEF`).
    Invalid,
    Eof,
}

impl Tok<'_> {
    /// How bison names the token in "syntax error, unexpected …" messages
    /// (the `%token` aliases in `parser.y`).
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Tok::Id(_) => "identifier",
            Tok::Int(_) => "integer",
            Tok::Float(_) => "floating-point literal",
            Tok::Str(_) => "string",
            Tok::IndStr { .. } => "indented string",
            Tok::Path(_) => "path",
            Tok::HPath(_) => "'~/…' path",
            Tok::SPath(_) => "'<…>' path",
            Tok::PathEnd => "end of path",
            Tok::Uri(_) => "URI",
            Tok::If => "'if'",
            Tok::Then => "'then'",
            Tok::Else => "'else'",
            Tok::Assert => "'assert'",
            Tok::With => "'with'",
            Tok::Let => "'let'",
            Tok::In => "'in'",
            Tok::Rec => "'rec'",
            Tok::Inherit => "'inherit'",
            Tok::OrKw => "'or'",
            Tok::Eq => "'=='",
            Tok::NEq => "'!='",
            Tok::Leq => "'<='",
            Tok::Geq => "'>='",
            Tok::Update => "'//'",
            Tok::Concat => "'++'",
            Tok::And => "'&&'",
            Tok::Or => "'||'",
            Tok::Impl => "'->'",
            Tok::PipeFrom => "'<|'",
            Tok::PipeInto => "'|>'",
            Tok::DollarCurly => "'${'",
            Tok::IndStringOpen => "start of an indented string",
            Tok::IndStringClose => "end of an indented string",
            Tok::Ellipsis => "'...'",
            Tok::Colon => "':'",
            Tok::At => "'@'",
            Tok::Semi => "';'",
            Tok::Assign => "'='",
            Tok::LBrace => "'{'",
            Tok::RBrace => "'}'",
            Tok::LBracket => "'['",
            Tok::RBracket => "']'",
            Tok::LParen => "'('",
            Tok::RParen => "')'",
            Tok::Dot => "'.'",
            Tok::Question => "'?'",
            Tok::Bang => "'!'",
            Tok::Minus => "'-'",
            Tok::Plus => "'+'",
            Tok::Star => "'*'",
            Tok::Slash => "'/'",
            Tok::Lt => "'<'",
            Tok::Gt => "'>'",
            Tok::Comma => "','",
            Tok::Quote => "'\"'",
            Tok::Invalid => "invalid token",
            Tok::Eof => "end of file",
        }
    }
}

/// A token with its byte range in the source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token<'a> {
    pub tok: Tok<'a>,
    pub start: u32,
    pub end: u32,
}

impl Token<'_> {
    pub(crate) fn pos(&self) -> Pos {
        Pos(self.start)
    }
}

/// Flex start conditions (`%s`/`%x` in `lexer.l`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// The bottom of the stack: like `Default`, except that a `}` does not
    /// pop it.
    Initial,
    Default,
    Str,
    IndStr,
    /// Inside a path literal, after a piece that does not end in `/`.
    InPath,
    /// Inside a path literal, right after a `/`: the path must continue.
    InPathSlash,
}

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(utf8 = false)]
enum DefaultTok {
    #[regex(r"(?-u:[ \t\r\n]+)")]
    Whitespace,
    #[regex(r"(?-u:#[^\r\n]*)", allow_greedy = true)]
    LineComment,
    #[regex(r"(?-u:/\*([^*]|\*+[^*/])*\*+/)", allow_greedy = true)]
    BlockComment,
    /// Identifiers and keywords; the driver tells them apart.
    #[regex("[a-zA-Z_][a-zA-Z0-9_'-]*")]
    Id,
    #[regex("[0-9]+")]
    Int,
    #[regex(r"(?-u:(([1-9][0-9]*\.[0-9]*)|(0?\.[0-9]+))([Ee][+-]?[0-9]+)?)")]
    Float,
    #[regex(r"(?-u:[a-zA-Z0-9._+-]*(/[a-zA-Z0-9._+-]+)+/?)")]
    Path,
    #[regex(r"(?-u:~(/[a-zA-Z0-9._+-]+)+/?)")]
    HPath,
    /// `{PATH_SEG}\$\{`: a path that starts with an interpolation right
    /// after its first slash, such as `./${x}` or `foo/${x}`.
    #[regex(r"(?-u:[a-zA-Z0-9._+-]*/\$\{)")]
    PathSegInterp,
    #[token("~/${")]
    HPathStartInterp,
    #[regex(r"(?-u:<[a-zA-Z0-9._+-]+(/[a-zA-Z0-9._+-]+)*>)")]
    SPath,
    #[regex(r"(?-u:[a-zA-Z][a-zA-Z0-9+.-]*:[a-zA-Z0-9%/?:@&=+$,_.!~*'-]+)")]
    Uri,
    #[token("...")]
    Ellipsis,
    #[token("==")]
    Eq,
    #[token("!=")]
    NEq,
    #[token("<=")]
    Leq,
    #[token(">=")]
    Geq,
    #[token("&&")]
    And,
    #[token("||")]
    Or,
    #[token("->")]
    Impl,
    #[token("//")]
    Update,
    #[token("++")]
    Concat,
    #[token("<|")]
    PipeFrom,
    #[token("|>")]
    PipeInto,
    #[token("${")]
    DollarCurly,
    #[token("}")]
    RBrace,
    #[token("{")]
    LBrace,
    #[token("\"")]
    Quote,
    /// `''` optionally followed by spaces and a newline, which are skipped.
    #[regex(r"(?-u:''( *\n)?)")]
    IndStringOpen,
    #[token(":")]
    Colon,
    #[token("@")]
    At,
    #[token(";")]
    Semi,
    #[token("=")]
    Assign,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(".")]
    Dot,
    #[token("?")]
    Question,
    #[token("!")]
    Bang,
    #[token("-")]
    Minus,
    #[token("+")]
    Plus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token(",")]
    Comma,
    /// Any other byte: flex's `{ANY}` rule returns it as a single-character
    /// token, which bison does not know and reports as an invalid token.
    #[regex(r"(?-u:[\x00-\xff])", priority = 0)]
    Other,
}

/// Inside `"…"`.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(utf8 = false)]
enum StrTok {
    /// Literal text: anything but `"`, `\` and `$`, or `$` not followed by
    /// `{`, or an escape, or `$` followed by an escape.
    #[regex(r#"(?-u:([^$"\\]|\$[^{"\\]|\\[\x00-\xff]|\$\\[\x00-\xff])+)"#)]
    Chunk,
    #[token("${")]
    DollarCurly,
    #[token("\"")]
    Quote,
    /// A `$` that `Chunk` could not take: it is followed by `"` or by the
    /// end of the input.
    #[token("$")]
    Dollar,
    /// Only matches at the end of the input (otherwise `Chunk` takes it).
    #[token("\\")]
    Backslash,
    /// Only matches at the end of the input.
    #[token("$\\")]
    DollarBackslash,
}

/// Inside `''…''`.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(utf8 = false)]
enum IndTok {
    #[regex(r"(?-u:([^$']|\$[^{']|'[^'$])+)")]
    Chunk,
    /// `''$` escapes `$`.
    #[token("''$")]
    QuoteQuoteDollar,
    /// A `$` not followed by `{`: literal.
    #[token("$")]
    Dollar,
    /// `'''` escapes `''`.
    #[token("'''")]
    TripleQuote,
    /// `''\x` escapes like `\x` in an ordinary string.
    #[regex(r"(?-u:''\\[\x00-\xff])")]
    Escape,
    #[token("${")]
    DollarCurly,
    #[token("''")]
    Close,
    /// A lone `'` (followed by `'`-free text or `$`): literal.
    #[token("'")]
    SingleQuote,
}

/// Inside a path literal, after its first piece.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(utf8 = false)]
enum PathTok {
    #[token("${")]
    DollarCurly,
    /// `{PATH}|{PATH_SEG}|{PATH_CHAR}+`
    #[regex(r"(?-u:[a-zA-Z0-9._+-]*(/[a-zA-Z0-9._+-]+)+/?|[a-zA-Z0-9._+-]*/|[a-zA-Z0-9._+-]+)")]
    Piece,
    /// Anything else ends the path (or is an error after a trailing slash).
    #[regex(r"(?-u:[\x00-\xff])", priority = 0)]
    Other,
}

/// The lexer driver. Produces one token per call to [`Lexer::next_token`];
/// the parser pulls tokens on demand, so lexer errors surface at the same
/// point of the parse as in Nix.
pub(crate) struct Lexer<'a> {
    src: &'a [u8],
    /// Offset of the next byte to lex.
    pos: usize,
    states: Vec<State>,
    /// The current token's range (`yylloc`), and the range of the token
    /// before it (`yylloc`'s stash), which Nix restores when it un-reads a
    /// character to end a path.
    loc: (usize, usize),
    stash: (usize, usize),
    pipe_operators: bool,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(src: &'a [u8], options: &ParseOptions) -> Self {
        Lexer {
            src,
            pos: 0,
            states: vec![State::Initial],
            loc: (0, 0),
            stash: (0, 0),
            pipe_operators: options.pipe_operators,
        }
    }

    fn state(&self) -> State {
        *self.states.last().expect("the state stack is never empty")
    }

    fn push(&mut self, state: State) {
        self.states.push(state);
    }

    fn pop(&mut self) {
        self.states.pop();
    }

    /// `adjustLoc` in `lexer-helpers.cc`: every matched rule, whitespace and
    /// comments included, advances the location.
    fn consume(&mut self, len: usize) {
        self.stash = self.loc;
        self.loc = (self.pos, self.pos + len);
        self.pos += len;
    }

    fn token(&self, tok: Tok<'a>) -> Token<'a> {
        Token {
            tok,
            start: self.loc.0 as u32,
            end: self.loc.1 as u32,
        }
    }

    fn eof(&self) -> Token<'a> {
        Token {
            tok: Tok::Eof,
            start: self.loc.1 as u32,
            end: self.loc.1 as u32,
        }
    }

    fn cur_pos(&self) -> Pos {
        Pos(self.loc.0 as u32)
    }

    fn rest(&self) -> &'a [u8] {
        &self.src[self.pos..]
    }

    /// Takes the text of the token just consumed.
    fn text(&self) -> &'a [u8] {
        &self.src[self.loc.0..self.loc.1]
    }

    pub(crate) fn next_token(&mut self) -> Result<Token<'a>, ParseError> {
        loop {
            let result = match self.state() {
                State::Initial | State::Default => self.lex_default()?,
                State::Str => self.lex_string()?,
                State::IndStr => self.lex_ind_string()?,
                State::InPath | State::InPathSlash => self.lex_path()?,
            };
            if let Some(token) = result {
                return Ok(token);
            }
        }
    }

    fn lex_default(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        let mut lexer = DefaultTok::lexer(self.rest());
        let Some(matched) = lexer.next() else {
            return Ok(Some(self.eof()));
        };
        let len = lexer.span().end.max(1);
        self.consume(len);
        let text = self.text();
        // `Other` matches any byte, so the lexer never fails; should it,
        // the failed input is an invalid token too.
        let matched = matched.unwrap_or(DefaultTok::Other);
        let tok = match matched {
            DefaultTok::Whitespace | DefaultTok::LineComment | DefaultTok::BlockComment => {
                return Ok(None)
            }
            DefaultTok::Id => keyword(text).unwrap_or(Tok::Id(text)),
            DefaultTok::Int => {
                let digits = std::str::from_utf8(text).expect("digits are ASCII");
                match digits.parse::<i64>() {
                    Ok(value) => Tok::Int(value),
                    Err(_) => {
                        return Err(ParseError::new(
                            format!("invalid integer '{digits}'"),
                            self.cur_pos(),
                        ))
                    }
                }
            }
            DefaultTok::Float => {
                let digits = std::str::from_utf8(text).expect("float literals are ASCII");
                let value = digits
                    .parse::<f64>()
                    .expect("the regex only matches valid floats");
                // strtod reports ERANGE on overflow and on underflow.
                let underflow = (value == 0.0 || value.is_subnormal())
                    && digits
                        .bytes()
                        .take_while(|&b| b != b'e' && b != b'E')
                        .any(|b| (b'1'..=b'9').contains(&b));
                if value.is_infinite() || underflow {
                    return Err(ParseError::new(
                        format!("invalid float '{digits}'"),
                        self.cur_pos(),
                    ));
                }
                Tok::Float(value)
            }
            DefaultTok::Path => {
                self.push(if text.ends_with(b"/") {
                    State::InPathSlash
                } else {
                    State::InPath
                });
                Tok::Path(text)
            }
            DefaultTok::HPath => {
                self.push(if text.ends_with(b"/") {
                    State::InPathSlash
                } else {
                    State::InPath
                });
                Tok::HPath(text)
            }
            DefaultTok::PathSegInterp | DefaultTok::HPathStartInterp => {
                // Flex re-reads this match as the path segment followed by
                // `${`: hand out the segment now, and let the path state
                // lex the `${` next.
                let seg_len = len - 2;
                self.pos = self.loc.0;
                self.loc = self.stash;
                self.consume(seg_len);
                self.push(State::InPathSlash);
                let seg = self.text();
                if matched == DefaultTok::HPathStartInterp {
                    Tok::HPath(seg)
                } else {
                    Tok::Path(seg)
                }
            }
            DefaultTok::SPath => Tok::SPath(text),
            DefaultTok::Uri => Tok::Uri(text),
            DefaultTok::Ellipsis => Tok::Ellipsis,
            DefaultTok::Eq => Tok::Eq,
            DefaultTok::NEq => Tok::NEq,
            DefaultTok::Leq => Tok::Leq,
            DefaultTok::Geq => Tok::Geq,
            DefaultTok::And => Tok::And,
            DefaultTok::Or => Tok::Or,
            DefaultTok::Impl => Tok::Impl,
            DefaultTok::Update => Tok::Update,
            DefaultTok::Concat => Tok::Concat,
            DefaultTok::PipeFrom | DefaultTok::PipeInto => {
                if !self.pipe_operators {
                    return Err(ParseError::new(
                        "experimental Nix feature 'pipe-operators' is disabled; add '--extra-experimental-features pipe-operators' to enable it",
                        self.cur_pos(),
                    ));
                }
                if matched == DefaultTok::PipeFrom {
                    Tok::PipeFrom
                } else {
                    Tok::PipeInto
                }
            }
            DefaultTok::DollarCurly => {
                self.push(State::Default);
                Tok::DollarCurly
            }
            DefaultTok::LBrace => {
                self.push(State::Default);
                Tok::LBrace
            }
            DefaultTok::RBrace => {
                if self.state() != State::Initial {
                    self.pop();
                }
                Tok::RBrace
            }
            DefaultTok::Quote => {
                self.push(State::Str);
                Tok::Quote
            }
            DefaultTok::IndStringOpen => {
                self.push(State::IndStr);
                Tok::IndStringOpen
            }
            DefaultTok::Colon => Tok::Colon,
            DefaultTok::At => Tok::At,
            DefaultTok::Semi => Tok::Semi,
            DefaultTok::Assign => Tok::Assign,
            DefaultTok::LBracket => Tok::LBracket,
            DefaultTok::RBracket => Tok::RBracket,
            DefaultTok::LParen => Tok::LParen,
            DefaultTok::RParen => Tok::RParen,
            DefaultTok::Dot => Tok::Dot,
            DefaultTok::Question => Tok::Question,
            DefaultTok::Bang => Tok::Bang,
            DefaultTok::Minus => Tok::Minus,
            DefaultTok::Plus => Tok::Plus,
            DefaultTok::Star => Tok::Star,
            DefaultTok::Slash => Tok::Slash,
            DefaultTok::Lt => Tok::Lt,
            DefaultTok::Gt => Tok::Gt,
            DefaultTok::Comma => Tok::Comma,
            // Nix's catch-all rule returns the byte as the token number,
            // and a NUL is token 0, the end of the input for Bison: the
            // rest of the source is never read.
            DefaultTok::Other if text == b"\0" => {
                self.pos = self.src.len();
                return Ok(Some(self.eof()));
            }
            DefaultTok::Other => Tok::Invalid,
        };
        Ok(Some(self.token(tok)))
    }

    fn lex_string(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        let mut lexer = StrTok::lexer(self.rest());
        let Some(matched) = lexer.next() else {
            return Ok(Some(self.eof()));
        };
        let len = lexer.span().end;
        let tok = match matched.expect("every byte is covered by a string-state rule") {
            StrTok::Chunk => {
                // Flex's trailing-context rule folds a `$` right before the
                // closing quote into the preceding piece.
                let len = if self.rest()[len..].starts_with(b"$\"") {
                    len + 1
                } else {
                    len
                };
                self.consume(len);
                Tok::Str(self.unescape(self.text())?)
            }
            StrTok::DollarCurly => {
                self.consume(len);
                self.push(State::Default);
                Tok::DollarCurly
            }
            StrTok::Quote => {
                self.consume(len);
                self.pop();
                Tok::Quote
            }
            StrTok::Dollar if self.rest()[len..].starts_with(b"\"") => {
                self.consume(len);
                Tok::Str(b"$".to_vec())
            }
            StrTok::Dollar | StrTok::Backslash | StrTok::DollarBackslash => {
                // Only possible at the end of the input; Nix hands the
                // problem to the parser as an end of file.
                self.consume(len);
                return Ok(Some(self.eof()));
            }
        };
        Ok(Some(self.token(tok)))
    }

    fn lex_ind_string(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        let mut lexer = IndTok::lexer(self.rest());
        let Some(matched) = lexer.next() else {
            return Ok(Some(self.eof()));
        };
        self.consume(lexer.span().end);
        let literal = |text: &[u8]| Tok::IndStr {
            text: text.to_vec(),
            has_indentation: false,
        };
        let tok = match matched.expect("every byte is covered by an indented-string rule") {
            IndTok::Chunk => {
                let text = self.text();
                self.check_no_null_byte(text)?;
                Tok::IndStr {
                    text: text.to_vec(),
                    has_indentation: true,
                }
            }
            IndTok::QuoteQuoteDollar | IndTok::Dollar => literal(b"$"),
            IndTok::TripleQuote => literal(b"''"),
            IndTok::Escape => Tok::IndStr {
                text: self.unescape(&self.text()[2..])?,
                has_indentation: false,
            },
            IndTok::DollarCurly => {
                self.push(State::Default);
                Tok::DollarCurly
            }
            IndTok::Close => {
                self.pop();
                Tok::IndStringClose
            }
            IndTok::SingleQuote => literal(b"'"),
        };
        Ok(Some(self.token(tok)))
    }

    fn lex_path(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        let mut lexer = PathTok::lexer(self.rest());
        let matched = lexer
            .next()
            .map(|m| m.expect("every byte is covered by a path-state rule"));
        let len = lexer.span().end;
        match matched {
            Some(PathTok::DollarCurly) => {
                self.consume(len);
                self.pop();
                self.push(State::InPath);
                self.push(State::Default);
                Ok(Some(self.token(Tok::DollarCurly)))
            }
            Some(PathTok::Piece) => {
                self.consume(len);
                let text = self.text();
                self.pop();
                self.push(if text.ends_with(b"/") {
                    State::InPathSlash
                } else {
                    State::InPath
                });
                Ok(Some(self.token(Tok::Str(text.to_vec()))))
            }
            Some(PathTok::Other) | None => {
                if self.state() == State::InPathSlash {
                    // The position is that of the offending character, or
                    // of the last token at the end of the input.
                    if matched.is_some() {
                        self.consume(len);
                    }
                    return Err(ParseError::new("path has a trailing slash", self.cur_pos()));
                }
                // The character is not consumed: it is re-read in the
                // enclosing state. Nix restores the previous token's
                // location when it un-reads (`yyless(0)` + `unstash`), so
                // the end-of-path token is positioned at the last path
                // piece, or at the token before it at the end of input.
                self.pop();
                let loc = if matched.is_some() {
                    self.loc
                } else {
                    self.stash
                };
                Ok(Some(Token {
                    tok: Tok::PathEnd,
                    start: loc.0 as u32,
                    end: loc.1 as u32,
                }))
            }
        }
    }

    /// `unescapeStr` in `lexer.l`: resolves `\n`, `\r`, `\t` and `\x`, and
    /// normalises CR and CRLF to LF. Null bytes are rejected afterwards.
    fn unescape(&self, s: &[u8]) -> Result<Vec<u8>, ParseError> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            let c = s[i];
            i += 1;
            match c {
                b'\\' => {
                    // The regexes guarantee a byte follows a backslash.
                    let c = s[i];
                    i += 1;
                    out.push(match c {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        c => c,
                    });
                }
                b'\r' => {
                    out.push(b'\n');
                    if s.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                }
                c => out.push(c),
            }
        }
        self.check_no_null_byte(&out)?;
        Ok(out)
    }

    /// `forceNoNullByte`: Nix strings cannot contain NUL. The message
    /// shows the string's bytes, each NUL as `␀`.
    fn check_no_null_byte(&self, s: &[u8]) -> Result<(), ParseError> {
        if s.contains(&0) {
            let mut msg = b"input string '".to_vec();
            for &b in s {
                match b {
                    0 => msg.extend_from_slice("␀".as_bytes()),
                    b => msg.push(b),
                }
            }
            msg.extend_from_slice(
                b"' cannot be represented as Nix string because it contains null bytes",
            );
            return Err(ParseError::new(msg, self.cur_pos()));
        }
        Ok(())
    }
}

fn keyword(text: &[u8]) -> Option<Tok<'static>> {
    Some(match text {
        b"if" => Tok::If,
        b"then" => Tok::Then,
        b"else" => Tok::Else,
        b"assert" => Tok::Assert,
        b"with" => Tok::With,
        b"let" => Tok::Let,
        b"in" => Tok::In,
        b"rec" => Tok::Rec,
        b"inherit" => Tok::Inherit,
        b"or" => Tok::OrKw,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Tok<'_>> {
        lex_with(src, &ParseOptions::default()).expect("lexes")
    }

    fn lex_with<'a>(src: &'a str, options: &ParseOptions) -> Result<Vec<Tok<'a>>, ParseError> {
        let mut lexer = Lexer::new(src.as_bytes(), options);
        let mut out = Vec::new();
        loop {
            let t = lexer.next_token()?;
            if t.tok == Tok::Eof {
                return Ok(out);
            }
            out.push(t.tok);
        }
    }

    fn s(text: &str) -> Tok<'static> {
        Tok::Str(text.as_bytes().to_vec())
    }

    fn ind(text: &str, has_indentation: bool) -> Tok<'static> {
        Tok::IndStr {
            text: text.as_bytes().to_vec(),
            has_indentation,
        }
    }

    #[test]
    fn keywords_and_identifiers() {
        assert_eq!(
            lex("if then else assert with let in rec inherit or if-x in' _a"),
            vec![
                Tok::If,
                Tok::Then,
                Tok::Else,
                Tok::Assert,
                Tok::With,
                Tok::Let,
                Tok::In,
                Tok::Rec,
                Tok::Inherit,
                Tok::OrKw,
                Tok::Id(b"if-x"),
                Tok::Id(b"in'"),
                Tok::Id(b"_a"),
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            lex("1 007 1. .5e3 1.e5 0.5"),
            vec![
                Tok::Int(1),
                Tok::Int(7),
                Tok::Float(1.0),
                Tok::Float(500.0),
                Tok::Float(100000.0),
                Tok::Float(0.5),
            ]
        );
        // `1e5` is an integer followed by an identifier.
        assert_eq!(lex("1e5"), vec![Tok::Int(1), Tok::Id(b"e5")]);
        assert_eq!(lex("0."), vec![Tok::Int(0), Tok::Dot]);
        let err = lex_with("99999999999999999999", &ParseOptions::default()).unwrap_err();
        assert_eq!(err.msg, "invalid integer '99999999999999999999'");
        assert_eq!(err.pos, Some(Pos(0)));
        let err = lex_with("1.5e400", &ParseOptions::default()).unwrap_err();
        assert_eq!(err.msg, "invalid float '1.5e400'");
    }

    #[test]
    fn longest_match_quirks() {
        // A colon makes a URI, a slash makes a path, `<…>` makes a search path.
        assert_eq!(lex("x:y"), vec![Tok::Uri(b"x:y")]);
        assert_eq!(lex("x: y"), vec![Tok::Id(b"x"), Tok::Colon, Tok::Id(b"y")]);
        assert_eq!(lex("1/2"), vec![Tok::Path(b"1/2"), Tok::PathEnd]);
        assert_eq!(lex("1./2"), vec![Tok::Path(b"1./2"), Tok::PathEnd]);
        assert_eq!(
            lex("a <b> c"),
            vec![Tok::Id(b"a"), Tok::SPath(b"<b>"), Tok::Id(b"c")]
        );
        assert_eq!(lex("a-b"), vec![Tok::Id(b"a-b")]);
        assert_eq!(lex("a - b"), vec![Tok::Id(b"a"), Tok::Minus, Tok::Id(b"b")]);
        assert_eq!(
            lex("foo/ bar"),
            vec![Tok::Id(b"foo"), Tok::Slash, Tok::Id(b"bar")]
        );
        assert_eq!(
            lex("foo.bar/baz"),
            vec![Tok::Path(b"foo.bar/baz"), Tok::PathEnd]
        );
    }

    #[test]
    fn strings() {
        assert_eq!(lex(r#""a\nb""#), vec![Tok::Quote, s("a\nb"), Tok::Quote]);
        assert_eq!(lex(r#""a$""#), vec![Tok::Quote, s("a$"), Tok::Quote]);
        assert_eq!(lex(r#""$""#), vec![Tok::Quote, s("$"), Tok::Quote]);
        assert_eq!(
            lex(r#""$$${""#),
            vec![Tok::Quote, s("$$"), Tok::DollarCurly, Tok::Quote]
        );
        assert_eq!(
            lex(r#""a${x}b$""#),
            vec![
                Tok::Quote,
                s("a"),
                Tok::DollarCurly,
                Tok::Id(b"x"),
                Tok::RBrace,
                s("b$"),
                Tok::Quote
            ]
        );
        assert_eq!(lex(r#""\${x}""#), vec![Tok::Quote, s("${x}"), Tok::Quote]);
        assert_eq!(
            lex("\"a\r\nb\rc\""),
            vec![Tok::Quote, s("a\nb\nc"), Tok::Quote]
        );
        assert_eq!(lex("\"\""), vec![Tok::Quote, Tok::Quote]);
        // A `$` at the end of the input is handed over as end of file.
        assert_eq!(lex("\"a$"), vec![Tok::Quote, s("a")]);
        assert_eq!(lex("\"a\\"), vec![Tok::Quote, s("a")]);
    }

    #[test]
    fn indented_strings() {
        assert_eq!(
            lex("''a''$b'''c''\\nd''"),
            vec![
                Tok::IndStringOpen,
                ind("a", true),
                ind("$", false),
                ind("b", true),
                ind("''", false),
                ind("c", true),
                ind("\n", false),
                ind("d", true),
                Tok::IndStringClose,
            ]
        );
        assert_eq!(
            lex("''  \n x'y$ ${z}''"),
            vec![
                Tok::IndStringOpen,
                ind(" x'y$ ", true),
                Tok::DollarCurly,
                Tok::Id(b"z"),
                Tok::RBrace,
                Tok::IndStringClose,
            ]
        );
        assert_eq!(
            lex("'''$''"),
            vec![
                Tok::IndStringOpen,
                ind("'", false),
                ind("$", false),
                Tok::IndStringClose
            ]
        );
    }

    #[test]
    fn paths() {
        assert_eq!(lex("./foo"), vec![Tok::Path(b"./foo"), Tok::PathEnd]);
        assert_eq!(
            lex("./${x}/y"),
            vec![
                Tok::Path(b"./"),
                Tok::DollarCurly,
                Tok::Id(b"x"),
                Tok::RBrace,
                s("/y"),
                Tok::PathEnd
            ]
        );
        assert_eq!(
            lex("foo/bar/${x}.c)"),
            vec![
                Tok::Path(b"foo/bar/"),
                Tok::DollarCurly,
                Tok::Id(b"x"),
                Tok::RBrace,
                s(".c"),
                Tok::PathEnd,
                Tok::RParen
            ]
        );
        assert_eq!(
            lex("./a//b"),
            vec![Tok::Path(b"./a/"), s("/b"), Tok::PathEnd]
        );
        let err = lex_with("~/foo/\n", &ParseOptions::default()).unwrap_err();
        assert_eq!(err.msg, "path has a trailing slash");
        assert_eq!(err.pos, Some(Pos(6)));
        let err = lex_with("a/b/", &ParseOptions::default()).unwrap_err();
        assert_eq!(err.pos, Some(Pos(0)));
        assert_eq!(lex("~/x"), vec![Tok::HPath(b"~/x"), Tok::PathEnd]);
        assert_eq!(
            lex("~/${x}"),
            vec![
                Tok::HPath(b"~/"),
                Tok::DollarCurly,
                Tok::Id(b"x"),
                Tok::RBrace,
                Tok::PathEnd
            ]
        );
        assert_eq!(lex("~"), vec![Tok::Invalid]);
    }

    #[test]
    fn braces_nest_through_interpolations() {
        assert_eq!(
            lex(r#""${{ a = "}"; }.a}""#),
            vec![
                Tok::Quote,
                Tok::DollarCurly,
                Tok::LBrace,
                Tok::Id(b"a"),
                Tok::Assign,
                Tok::Quote,
                s("}"),
                Tok::Quote,
                Tok::Semi,
                Tok::RBrace,
                Tok::Dot,
                Tok::Id(b"a"),
                Tok::RBrace,
                Tok::Quote,
            ]
        );
        // An unmatched `}` at the top level is still a token.
        assert_eq!(lex("}"), vec![Tok::RBrace]);
    }

    #[test]
    fn comments_and_invalid_bytes() {
        assert_eq!(
            lex("1 # c\n/* a ** b */ /** doc */ /**/ 2"),
            vec![Tok::Int(1), Tok::Int(2)]
        );
        assert!(lex("é").iter().all(|t| *t == Tok::Invalid));
        assert_eq!(lex("1 $ 2"), vec![Tok::Int(1), Tok::Invalid, Tok::Int(2)]);
    }

    #[test]
    fn a_nul_ends_the_input_outside_strings_and_comments() {
        assert_eq!(lex("1 \0 2"), vec![Tok::Int(1)]);
        assert_eq!(lex("1 # \0\n2"), vec![Tok::Int(1), Tok::Int(2)]);
    }

    #[test]
    fn pipe_operators_need_the_feature() {
        let err = lex_with("a |> b", &ParseOptions::default()).unwrap_err();
        assert!(err
            .msg
            .starts_with(b"experimental Nix feature 'pipe-operators' is disabled"));
        assert_eq!(err.pos, Some(Pos(2)));
        let on = ParseOptions {
            pipe_operators: true,
            ..ParseOptions::default()
        };
        assert_eq!(
            lex_with("a |> b <| c", &on).unwrap(),
            vec![
                Tok::Id(b"a"),
                Tok::PipeInto,
                Tok::Id(b"b"),
                Tok::PipeFrom,
                Tok::Id(b"c")
            ]
        );
    }

    #[test]
    fn null_bytes_are_rejected() {
        let err = lex_with("\"a\0b\"", &ParseOptions::default()).unwrap_err();
        assert_eq!(
            err.msg,
            "input string 'a␀b' cannot be represented as Nix string because it contains null bytes"
        );
        assert_eq!(err.pos, Some(Pos(1)));
    }

    #[test]
    fn positions() {
        let mut lexer = Lexer::new(b"  ab  \"x\"", &ParseOptions::default());
        let t = lexer.next_token().unwrap();
        assert_eq!((t.tok, t.start, t.end), (Tok::Id(b"ab"), 2, 4));
        let t = lexer.next_token().unwrap();
        assert_eq!((t.tok, t.start, t.end), (Tok::Quote, 6, 7));
        let t = lexer.next_token().unwrap();
        assert_eq!((t.tok, t.start, t.end), (s("x"), 7, 8));
        lexer.next_token().unwrap();
        let t = lexer.next_token().unwrap();
        assert_eq!((t.tok, t.start, t.end), (Tok::Eof, 9, 9));
    }
}
