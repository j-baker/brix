//! brix: a bug-for-bug compatible Nix evaluator, as a library.
//!
//! A [`Session`] owns one evaluator. It evaluates expressions and flakes
//! (pure mode, locked inputs only), and records everything evaluation
//! "adds to the store" in a virtual store that the caller reads back:
//! derivations (with their `.drv` text), source trees (with their NAR
//! hashes) and text files. Nothing is written to a real store by
//! evaluation; a Nix daemon is used, lazily, when a build or a store query
//! is asked for (import-from-derivation, [`Session::store_realise`]).
//!
//! ```no_run
//! use brix::{Config, Format, Session};
//!
//! let session = Session::new(Config::default())?;
//! let flake_ref = session.parse_flake_ref("github:NixOS/nixpkgs/nixos-25.05", "/")?;
//! let flake = session.flake(&flake_ref)?;
//! let drv_path = flake.eval_attr_to_string(
//!     "legacyPackages.x86_64-linux.hello.drvPath",
//!     Format::Raw,
//! )?;
//! for (path, drv) in session.derivations() {
//!     // The text is bytes: a derivation's strings need not be UTF-8.
//!     let text = drv.to_aterm(session.config().store_dir())?;
//!     println!("{path}: {}", String::from_utf8_lossy(&text));
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(brix_parallel)]
mod batch;
mod callflake;
#[cfg(brix_parallel)]
pub use batch::{BatchContext, BatchCursor, BatchValue};
pub mod compiled;
pub mod config;
pub mod cursor;
pub mod derivation_json;
pub mod error;
mod fetching;
mod flake;
mod host;
pub mod log;
pub mod nixconf;
mod root_fs;
pub mod schemas;
mod session;
pub mod store;
mod value_slots;

pub use brix_eval::cpus::performance_threads;
pub use brix_eval::parse_drv_name_str as parse_drv_name;
pub use brix_eval::value::ValueType;
pub use brix_eval::{best_suggestions, format_suggestions, scored_suggestions};
pub use brix_flake::{FlakeRef, LockFlags, LockedFlake};
pub use brix_store::{Derivation, DerivationOutput, StoreDir, StoreObject, StorePath};
pub use compiled::CompiledImage;
pub use config::{host_system, BuildLogMode, CompiledInputsConfig, Config, FetchConfig, Teardown};
pub use cursor::{attr_path_to_string, parse_attr_path, ContextElem, Cursor};
pub use error::{Error, Result};
pub use flake::{TreeHandle, TreeRoot};
pub use session::{AutoArg, Flake, Format, Prefetched, Session, Value};
