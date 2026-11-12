//! Lexer and parser for the Nix expression language.
//!
//! The crate reproduces Nix's own parser (`lexer.l`, `parser.y`) closely
//! enough that the AST it produces, printed with [`show`], is byte for byte
//! what `nix-instantiate --parse` prints, and the errors and warnings it
//! reports, rendered with [`ParseError::render`] and [`Warning::render`],
//! are what `nix` prints to stderr. Undefined variables are not found
//! here: Nix reports them when it binds variables after parsing, which
//! brix does when it compiles the AST (`brix-eval`).
//!
//! The [`ast`] module is the contract with the evaluator. Parsing does no
//! I/O: the caller supplies the source bytes and the directory that
//! relative path literals are resolved against, and supplies the origin
//! when it shows an error or a warning. Paths are bytes, as in Nix: the
//! directory, the resolved path literals ([`ast::Expr::Path`]) and a
//! file's origin ([`Origin::Path`]) need not be UTF-8, and are shown as
//! they are.
//!
//! ```
//! use brix_syntax::{parse, show, ParseOptions};
//!
//! let mut warnings = Vec::new();
//! let expr = parse(b"{ a = 1; }.a", "/pwd", &ParseOptions::default(), &mut warnings).unwrap();
//! assert_eq!(show(&expr), "({ a = 1; }).a");
//! assert!(warnings.is_empty());
//! ```

#![forbid(unsafe_code)]

pub mod ast;
pub mod error;
pub mod float;
mod lexer;
pub mod parser;
pub mod path;
pub mod printer;
pub mod source;

pub use error::{ParseError, Warning};
pub use parser::{parse, Diagnose, Lints, ParseOptions};
pub use printer::{show, show_bytes};
pub use source::{Origin, SourceFile};

/// Remaining stack below which recursive routines request a new segment.
pub(crate) const STACK_RED_ZONE: usize = 128 * 1024;
/// Size of each stack segment `stacker` allocates on demand.
pub(crate) const STACK_SEGMENT: usize = 8 * 1024 * 1024;
