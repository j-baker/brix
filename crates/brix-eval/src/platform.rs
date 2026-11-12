//! What the evaluator needs from the platform beyond memory: a clock for
//! the phase timings ([`crate::Stats`]) and threads for compiling images
//! (`image/writer.rs`). Everything else outside the process goes through
//! the [`crate::Host`].
//!
//! `wasm32-unknown-unknown` has neither: `std::time::Instant::now`
//! panics there and threads cannot be spawned. On that target the timings
//! are zero (and [`crate::Stats::render`] says so) and images are compiled
//! on the calling thread. Elsewhere [`Instant`] is std's, so the timing
//! code is what it would be without this module.

/// `wasm32-unknown-unknown`, or any other wasm target without an
/// operating system.
const BARE_WASM: bool = cfg!(all(target_family = "wasm", target_os = "unknown"));

/// Whether [`Instant`] measures time. Without a clock every duration is
/// zero.
pub(crate) const HAS_CLOCK: bool = !BARE_WASM;

/// Whether threads can be spawned.
pub(crate) const HAS_THREADS: bool = !BARE_WASM;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use std::time::Instant;

/// A clock that never advances, for targets without one.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Instant;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Instant {
    pub(crate) fn now() -> Instant {
        Instant
    }

    pub(crate) fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
