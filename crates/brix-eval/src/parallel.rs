//! Native parallel evaluation infrastructure.
//!
//! These types are deliberately separate from the serial machine: neither a
//! value nor an allocation in that machine pays for their synchronization.
//! A worker runs one synchronous evaluation at a time. `derivationStrict`
//! offers its dependencies to idle workers as background sparks. Waiting
//! workers never execute another job on their own stack, preserving thunk
//! ownership.

#![allow(dead_code)] // Integrated with the parallel machine separately.

#[cfg(brix_parallel)]
pub(crate) mod jobs;
pub(crate) mod publication;
#[cfg(brix_parallel)]
pub(crate) mod sparks;
pub(crate) mod wait;
pub(crate) mod world;

use std::num::NonZeroU32;

/// A worker identity within one evaluation runtime. Zero is reserved for
/// unpublished cells. Slots keep their identity across batches and may be
/// reattached only after the previous batch has joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkerId(NonZeroU32);

impl WorkerId {
    pub fn from_index(index: usize) -> Self {
        let raw = u32::try_from(index).expect("worker index fits u32");
        assert!(raw < publication::MAX_WORKERS);
        Self(NonZeroU32::new(raw + 1).unwrap())
    }

    pub fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }

    fn get(self) -> u32 {
        self.0.get()
    }
}
