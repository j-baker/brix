//! The ownership/publication word of a parallel thunk.
//!
//! This word does not own the payload. Its caller may read a suspended payload
//! only after successfully claiming it, and a completed payload only after an
//! acquire read observes Ready. While a worker owns a cell, it must keep the
//! original suspension rooted until it either publishes a result or restores
//! the suspension on unwind. GC may relocate a cell only with all mutators
//! stopped; no reference to this word may survive a parking boundary.

use std::sync::atomic::{AtomicU32, Ordering};

use super::WorkerId;

const EMPTY: u32 = 0;
const READY: u32 = u32::MAX;
const AWAITED: u32 = 1 << 31;
const OWNER_MASK: u32 = AWAITED - 1;
const INSPECTING: u32 = OWNER_MASK;
pub(crate) const MAX_WORKERS: u32 = OWNER_MASK - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Suspended,
    Running(WorkerId),
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    Acquired,
    Running(WorkerId),
    Ready,
}

/// Four bytes; wait records live outside the heap and exist only on contention.
#[repr(transparent)]
pub(crate) struct Publication(AtomicU32);

impl Publication {
    pub const fn suspended() -> Self {
        Self(AtomicU32::new(EMPTY))
    }

    pub const fn ready() -> Self {
        Self(AtomicU32::new(READY))
    }

    fn decode(word: u32) -> State {
        match word {
            EMPTY => State::Suspended,
            READY => State::Ready,
            INSPECTING => unreachable!("inspection is retried before decoding"),
            _ => State::Running(WorkerId::from_index((word & OWNER_MASK) as usize - 1)),
        }
    }

    /// The common case of every read: a published value.
    #[inline(always)]
    pub fn is_published(&self) -> bool {
        self.0.load(Ordering::Acquire) == READY
    }

    #[inline]
    pub fn state(&self) -> State {
        loop {
            let word = self.0.load(Ordering::Acquire);
            if word != INSPECTING {
                return Self::decode(word);
            }
            std::hint::spin_loop();
        }
    }

    /// Claim a suspension, or observe its owner/completion. Acquiring the word
    /// also observes a suspension restored by a previous owner after an unwind.
    #[inline]
    pub fn claim(&self, worker: WorkerId) -> Claim {
        loop {
            return match self.0.compare_exchange(
                EMPTY,
                worker.get(),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => Claim::Acquired,
                Err(INSPECTING) => {
                    std::hint::spin_loop();
                    continue;
                }
                Err(word) => match Self::decode(word) {
                    State::Running(owner) => Claim::Running(owner),
                    State::Ready => Claim::Ready,
                    State::Suspended => unreachable!("strong CAS failed with the expected value"),
                },
            };
        }
    }

    /// Inspect an unclaimed suspension without racing its evaluator. The
    /// callback must only copy the payload: never block, allocate, or reenter.
    /// Completed/running cells are returned to the caller for classification.
    pub fn inspect<T>(&self, read: impl FnOnce() -> T) -> Result<T, State> {
        loop {
            match self
                .0
                .compare_exchange(EMPTY, INSPECTING, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => {
                    struct Restore<'a>(&'a AtomicU32, u32);
                    impl Drop for Restore<'_> {
                        fn drop(&mut self) {
                            self.0.store(self.1, Ordering::Release);
                        }
                    }
                    let _restore = Restore(&self.0, EMPTY);
                    return Ok(read());
                }
                Err(INSPECTING) => std::hint::spin_loop(),
                Err(word) => return Err(Self::decode(word)),
            }
        }
    }

    /// Called under the wait registry's lock, while the mutator still protects
    /// the cell against relocation. Completion must notify that same registry
    /// when this bit was set. A changed owner/completed cell means retry.
    pub fn mark_awaited(&self, owner: WorkerId) -> bool {
        let word = owner.get();
        match self
            .0
            .compare_exchange(word, word | AWAITED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(actual) => actual == word | AWAITED,
        }
    }

    /// Publish an initialized result (including a memoized failure). The return
    /// value says whether to notify the wait registry. No registry access is
    /// needed for an uncontended evaluation.
    #[inline]
    pub fn finish(&self, owner: WorkerId) -> bool {
        self.release(owner, READY)
    }

    /// Publish the restored original suspension after an unwind. Restoration
    /// and root/write-barrier handling precede this operation.
    pub fn abandon(&self, owner: WorkerId) -> bool {
        self.release(owner, EMPTY)
    }

    #[inline]
    fn release(&self, owner: WorkerId, next: u32) -> bool {
        // Only the owner changes the payload/state, except for the waited bit.
        // Check ownership before changing the word even in release builds.
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            assert_eq!(
                current & OWNER_MASK,
                owner.get(),
                "only the owner may publish a thunk"
            );
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed)
            {
                Ok(previous) => return previous & AWAITED != 0,
                Err(actual) => current = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_publication_and_retry() {
        let a = WorkerId::from_index(0);
        let b = WorkerId::from_index(1);
        let cell = Publication::suspended();
        assert_eq!(std::mem::size_of::<Publication>(), 4);
        assert_eq!(cell.claim(a), Claim::Acquired);
        assert_eq!(cell.claim(b), Claim::Running(a));
        assert!(!cell.mark_awaited(b));
        assert!(cell.mark_awaited(a));
        assert!(cell.abandon(a));
        assert_eq!(cell.claim(b), Claim::Acquired);
        assert!(!cell.finish(b));
        assert_eq!(cell.claim(a), Claim::Ready);
        assert!(!cell.mark_awaited(b));
    }
}
