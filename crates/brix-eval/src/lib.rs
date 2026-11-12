//! The Nix evaluator.
//!
//! See `docs/ARCHITECTURE.md`. In short: the compiler (`compile.rs`) turns
//! the parser's AST into a flat, scope-resolved [`ir::Code`]; the
//! [`Evaluator`] loads it into its arena and evaluates it with Nix's own
//! runtime model (cells, thunks, environments); `builtins/` implements the
//! `builtins` set; the printers (`print.rs`, [`json`], `xml.rs`) render
//! values the way Nix does.
//!
//! Only the modules other crates need are public; everything else is
//! reached through the re-exports below and the methods of [`Evaluator`].
//!
//! ```
//! use brix_eval::{Arena, Config, Evaluator, NullHost};
//!
//! let arena = Arena::new();
//! let host = NullHost;
//! let ev = Evaluator::new(&arena, Config::default(), &host);
//! let v = ev.eval_str(b"let x = 2; in x * 21", "/").unwrap();
//! assert_eq!(ev.print_ambiguous(ev.alloc_cell(v)).unwrap(), "42");
//! ```

// The only places that need `unsafe` are the managed heap (`heap_layout.rs`
// and `managed_heap.rs`, whose allowance also covers its submodules
// `managed_heap/weak_seen.rs`, the tests, and, in the parallel build,
// `parallel/heap.rs`), `GcEvaluator::new` in
// `managed_api.rs`, and, in the parallel build only, the locked arena and
// `Published::get` in `runtime.rs`; everything else must do without.
#![deny(unsafe_code)]

mod bindings;
mod builtins;
mod closure;
mod code;
mod coerce;
mod compile;
pub mod cpus;
mod env;
mod error;
mod eval;
mod evaluator;
mod grisu;
mod heap_layout;
mod host;
pub mod image;
mod import;
pub mod ir;
pub mod json;
pub mod lazy_tree;
pub mod managed_api;
pub(crate) mod managed_bindings;
pub(crate) mod managed_builtins;
pub(crate) mod managed_builtins_data;
pub(crate) mod managed_builtins_derivation;
pub(crate) mod managed_builtins_fetch;
pub(crate) mod managed_builtins_paths;
pub(crate) mod managed_builtins_strings;
pub(crate) mod managed_eval;
pub(crate) mod managed_heap;
pub(crate) mod managed_import;
pub(crate) mod managed_language;
pub(crate) mod managed_print;
mod mount;
mod owned;
#[cfg(all(not(target_family = "wasm"), any(brix_parallel, test)))]
mod parallel;
mod platform;
mod print;
mod regex;
mod runtime;

/// Storage types used by the native parallel host implementation.
#[doc(hidden)]
#[cfg(brix_parallel)]
pub mod parallel_support {
    pub use crate::runtime::cell;
}
mod show;
mod stack;
mod stats;
pub mod string_context;
mod symbol;
mod terminal;
mod toml_datetime;
pub mod value;
mod xml;
mod xml_writer;

/// The arena every run-time value lives in. Dropping it frees them all.
pub type Arena = runtime::Bump;

pub use brix_syntax::printer::print_attribute_name;
pub use builtins::strings::{parse_drv_name, parse_drv_name_str};
pub use code::LinkedCode;
pub use coerce::Coercion;
pub use error::{
    best_suggestions, best_suggestions_bytes, format_suggestions, format_suggestions_bytes,
    scored_suggestions, scored_suggestions_bytes, Error, ErrorKind, EvalError, Position,
};
pub use evaluator::{Config, Evaluator};
pub use host::{
    BuildRequest, EvalStoreView, FetchAttr, FetchError, FetchRequest, FetchedFile, FetchedTree,
    Host, NullHost, SourceNar,
};
pub use ir::Origin;
pub use lazy_tree::LazyTreeId;
pub use mount::DisplayPrefixAccessor;
pub use owned::OwnedValue;
pub use print::PrintOptions;
pub use stats::Stats;
pub use symbol::Symbol;
#[doc(hidden)]
pub use symbol::STATIC_SYMBOLS;
pub use terminal::{filter_ansi_escapes, filter_ansi_escapes_str, indent_message};
pub use value::{ContextElem, SrcPos, Value, ValueRef, ValueType};

/// Errors are boxed: they are rare and large.
pub type Result<T> = std::result::Result<T, Error>;
