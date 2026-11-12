//! Precise moving garbage collector used by the brix evaluator.
//!
//! The crate has two parts:
//!
//! - [`raw`]: the generational collector. [`raw::RawHeap`] owns a bump-allocated
//!   nursery and a compacting old generation, and traces objects through
//!   caller-supplied [`raw::Descriptor`] tables. Its API is `unsafe`: the
//!   integrating runtime is responsible for object layout and rooting.
//! - [`roots`]: the scoped root registry that reports mutator roots to a
//!   collection.
//!
//! [`Config`], [`Error`], [`Result`] and [`Stats`] are shared by both.

#![deny(unsafe_op_in_unsafe_fn)]

// The tracer ranks a bitmap word on every edge into variable-size nursery
// storage. Without POPCNT that is a dozen instructions instead of one.
// rustdoc reads this file without the build's code generation flags.
#[cfg(all(target_arch = "x86_64", not(target_feature = "popcnt"), not(doc)))]
compile_error!(
    "brix-gc requires POPCNT on x86-64: build with `-C target-feature=+popcnt` \
     (.cargo/config.toml sets it; an explicit RUSTFLAGS must repeat it)"
);

pub mod raw;
pub mod roots;

use std::fmt;
use std::time::Duration;

/// Collector tuning. Thresholds are soft triggers, not process memory caps.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub chunk_bytes: usize,
    /// Maximum GC participants, including the calling thread. Zero selects
    /// automatically (up to eight); one keeps collection serial. Raw callers
    /// must separately opt into worker-safe tracing. Ignored on wasm. Explicit
    /// counts are capped at 64.
    pub workers: usize,
    /// Initial nursery budget.
    pub threshold_bytes: usize,
    /// Nursery budget after the first successful collection. `None` keeps
    /// `threshold_bytes` for every nursery cycle.
    pub steady_threshold_bytes: Option<usize>,
    /// Emit per-collection diagnostics to stderr.
    pub trace_collections: bool,
    /// Collect before every managed allocation (correctness testing only).
    pub stress: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chunk_bytes: 64 * 1024,
            workers: 0,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1024 * 1024,
            stress: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    AllocationTooLarge,
    OutOfMemory,
    InvalidRange,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AllocationTooLarge => "GC allocation is too large",
            Self::OutOfMemory => "GC backing allocation failed",
            Self::InvalidRange => "byte slice is outside its buffer",
        })
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

/// Counters are updated on collection, not on every allocation.
#[derive(Default, Clone, Debug)]
pub struct Stats {
    /// Participants selected for the last completed collection.
    pub last_collection_workers: usize,
    pub parallel_collections: usize,
    pub collections: usize,
    /// Nursery collections in the generational raw backend.
    pub minor_collections: usize,
    /// Full collections in the generational raw backend.
    pub major_collections: usize,
    /// Bytes moved by promotion/copying and old-generation compaction.
    pub copied_bytes: usize,
    /// Subset of copied bytes moved within the old generation.
    pub compacted_bytes: usize,
    /// Bytes copied from the nursery to the old generation.
    pub promoted_bytes: usize,
    /// Old-generation garbage reclaimed by major collections.
    pub old_reclaimed_bytes: usize,
    pub reclaimed_bytes: usize,
    /// Retained occupancy; after a minor cycle this includes unswept old garbage.
    pub last_live_bytes: usize,
    /// Worst backing capacity during a collection, including forwarding scratch
    /// but excluding root storage and allocator overhead.
    pub peak_collection_capacity: usize,
    pub collection_time: Duration,
    pub minor_collection_time: Duration,
    pub major_collection_time: Duration,
}
