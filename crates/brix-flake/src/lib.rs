//! Flake references, lock files, input fetching and the data needed to
//! call a flake, for brix.
//!
//! See `docs/ARCHITECTURE.md`. This crate mirrors Nix's `libflake` and
//! `libfetchers`: the same URL grammar, the same lock file format, the same
//! rules for what counts as a locked input. Nothing here evaluates Nix
//! code: the `brix` facade combines this crate with the evaluator.

#![forbid(unsafe_code)]

pub mod attrs;
pub mod call_flake;
pub mod error;
pub mod fetch;
pub mod fingerprint;
pub mod flake;
pub mod flakeref;
pub mod hashes;
pub mod input;
pub mod json;
pub mod lockfile;
#[cfg(feature = "native")]
pub mod native;
pub mod progress;
pub mod registry;
pub mod url;

pub use attrs::{Attr, Attrs};
pub use call_flake::{
    call_flake_args, prefetch_plan, source_info, CallFlakeArgs, CallFlakeOverride, SourceInfo,
};
pub use error::{Error, Result};
pub use fetch::{fetch_input, fetch_input_unhashed, FetchedTree, Fetcher, NoFetcher};
pub use flake::{
    lock_flake, parse_flake_metadata, FetchedInput, Flake, FlakeAttr, FlakeDir, FlakeInput,
    FlakeInputs, FlakeMetadata, FlakeValue, LockDeps, LockFlags, LockWrite, LockedFlake,
};
pub use flakeref::{parse_flake_ref, parse_flake_ref_with_fragment, FlakeRef, ParseOptions};
pub use hashes::{GitRev, NarHash};
pub use input::{FetchSettings, Input, Scheme, Warn};
pub use lockfile::{Edge, InputAttrPath, LockFile, LockedNode, Node, NodeId};
#[cfg(feature = "native")]
pub use native::{reuse_from_store, FetchCache, NativeFetcher};
pub use progress::{Activity, ActivityId, ActivityKind, NoProgress, Progress, Waiting};
pub use registry::{lookup_in_registries, Registry, RegistryType, UseRegistries};
