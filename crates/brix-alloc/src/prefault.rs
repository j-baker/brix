//! Pre-faulting of large allocations.
//!
//! Evaluation allocates almost everything in a bump arena whose chunks
//! double in size, so a nixpkgs evaluation touches hundreds of megabytes
//! of fresh memory one page at a time: about 9% of `firefox.drvPath` was
//! kernel time taking first-touch page faults. Populating a chunk's pages
//! up front costs the same zeroing but none of the per-page traps, and
//! doing it on a helper thread takes it off the evaluation's critical
//! path altogether.
//!
//! [`Prefaulting`] wraps any global allocator and, for allocations of at
//! least [`THRESHOLD`] bytes, asks the kernel to populate the pages with
//! `madvise(MADV_POPULATE_WRITE)` (Linux 5.14+). bumpalo hands out memory
//! from the *end* of a chunk towards its start, so populating runs from
//! the top down and covers at most [`WINDOW`] bytes of a chunk: the part
//! that is used first. Everything below the window faults lazily as
//! before, which bounds the resident memory a huge, mostly unused final
//! chunk can cost.
//!
//! Nothing is populated until [`set_policy`] (or `brix_alloc::init`)
//! selects a policy: that is where the kernel's support is checked and the
//! helper thread is started, so the allocation path itself never spawns,
//! allocates, blocks or fails. Library crates never install this; the
//! `brix` binary and the measurement examples do, with `mimalloc`
//! underneath. Other targets than Linux have no `MADV_POPULATE_WRITE`, and
//! only [`Policy::Off`] is available there.
//!
//! Measured on the nixpkgs workloads (release, in process, best of 3,
//! glibc malloc without pre-faulting as the baseline): `firefox.drvPath`
//! 789 → 680 ms and 118k → 15k page faults, a NixOS `toplevel` 2170 →
//! 1905 ms and 325k → 87k faults, `hello.drvPath` 156 → 144 ms.
//! Populating synchronously ([`Policy::Sync`]) recovers only a third of
//! that, because the zeroing then stays on the evaluating thread.
//! Transparent huge pages would remove the faults too, but with the
//! kernel's default `defrag=madvise` they stall on compaction once free
//! memory is fragmented (the same NixOS evaluation took 3.0 s), so the
//! binary builds mimalloc with `no_thp` and relies on this instead.

use std::alloc::{GlobalAlloc, Layout};
use std::ops::Range;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Allocations smaller than this are never pre-faulted: the arena's early
/// chunks and ordinary `Vec`s cost nothing to fault lazily.
pub const THRESHOLD: usize = 1 << 20;

/// At most this much of one allocation is populated, from its top down.
pub const WINDOW: usize = 256 << 20;

/// Pages are populated in slices of this size so that the top of a chunk
/// is usable before the rest has been zeroed. A multiple of every page
/// size Linux uses, so slices of a page-aligned window stay page-aligned.
const SLICE: usize = 8 << 20;

/// How large allocations are pre-faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Policy {
    /// Never populate; pages fault in on first touch.
    Off = 0,
    /// Populate synchronously in the allocating thread.
    Sync = 1,
    /// Populate on a helper thread, from the top of the allocation down.
    Background = 2,
}

impl Policy {
    /// Parses the spelling accepted by `BRIX_PREFAULT`.
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "off" => Some(Policy::Off),
            "sync" => Some(Policy::Sync),
            "background" => Some(Policy::Background),
            _ => None,
        }
    }

    /// The spelling `BRIX_PREFAULT` uses for this policy.
    pub fn name(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Sync => "sync",
            Policy::Background => "background",
        }
    }
}

/// The policy in force. Stored with `Release` after [`PAGE_SIZE`], so a
/// thread that sees a populating policy also sees the page size.
static POLICY: AtomicU8 = AtomicU8::new(Policy::Off as u8);

/// The page size, from `sysconf(_SC_PAGESIZE)`; set by [`set_policy`]
/// before any policy that populates is in force, 0 until then.
static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Selects the policy for every [`Prefaulting`] allocator in the process.
/// Binaries do this once at start-up through `brix_alloc::init`; until
/// then the policy is [`Policy::Off`].
///
/// [`Policy::Sync`] and [`Policy::Background`] need the kernel to support
/// `MADV_POPULATE_WRITE`, and [`Policy::Background`] starts the helper
/// thread the first time it is selected; either failing is an error that
/// says why, and leaves the policy unchanged.
pub fn set_policy(policy: Policy) -> Result<(), String> {
    if policy != Policy::Off {
        let page = populate_support()?;
        PAGE_SIZE.store(page, Ordering::Relaxed);
    }
    if policy == Policy::Background {
        background::start()?;
    }
    POLICY.store(policy as u8, Ordering::Release);
    Ok(())
}

/// The policy in force.
pub fn policy() -> Policy {
    match POLICY.load(Ordering::Acquire) {
        0 => Policy::Off,
        1 => Policy::Sync,
        _ => Policy::Background,
    }
}

/// Whether the kernel can populate pages: `Ok` with the page size, or why
/// not. Probed by populating one freshly mapped page.
#[cfg(target_os = "linux")]
pub(crate) fn populate_support() -> Result<usize, String> {
    let page = rustix::param::page_size();
    if !page.is_power_of_two() {
        return Err(format!("invalid page size: {page}"));
    }
    let mapping = memmap2::MmapMut::map_anon(page)
        .map_err(|error| format!("cannot map a page to probe for MADV_POPULATE_WRITE: {error}"))?;
    // The mapping owns the probe page and unmaps it on every return path.
    match mapping.advise(memmap2::Advice::PopulateWrite) {
        Ok(()) => Ok(page),
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Err(
            "this kernel does not support madvise(MADV_POPULATE_WRITE), which needs Linux 5.14 or later"
                .to_owned(),
        ),
        Err(error) => Err(format!("madvise(MADV_POPULATE_WRITE) fails: {error}")),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn populate_support() -> Result<usize, String> {
    Err("pre-faulting needs madvise(MADV_POPULATE_WRITE), which only Linux has".to_owned())
}

/// A global allocator that pre-faults large allocations made through
/// `inner`.
pub struct Prefaulting<A> {
    inner: A,
}

impl<A> Prefaulting<A> {
    pub const fn new(inner: A) -> Self {
        Prefaulting { inner }
    }
}

// SAFETY: every method forwards to `inner` unchanged; populating pages
// neither moves nor writes memory, so the returned blocks are exactly
// what `inner` handed out.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Prefaulting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.inner.alloc(layout) };
        if !p.is_null() {
            if let Some((start, len)) = fresh_range(p as usize, 0, layout.size()) {
                prefault(start, len);
            }
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.inner.alloc_zeroed(layout) };
        if !p.is_null() {
            if let Some((start, len)) = fresh_range(p as usize, 0, layout.size()) {
                prefault(start, len);
            }
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if let Some((start, len)) = fresh_range(p as usize, layout.size(), new_size) {
                prefault(start, len);
            }
        }
        p
    }
}

/// The part of a block of `new_size` bytes at `p` that has not been
/// written yet and is worth pre-faulting, as `(start, len)`: all of a new
/// block (`old_size` 0), or the growth of a reallocated one (the copied
/// prefix has just been written). `None` below [`THRESHOLD`].
#[inline]
fn fresh_range(p: usize, old_size: usize, new_size: usize) -> Option<(usize, usize)> {
    (new_size >= THRESHOLD && new_size > old_size).then(|| (p + old_size, new_size - old_size))
}

/// Populates the top of `[start, start + len)` according to the policy.
fn prefault(start: usize, len: usize) {
    match policy() {
        Policy::Off => {}
        Policy::Sync => populate_window(start, len),
        Policy::Background => background::enqueue(start, len),
    }
}

/// Populates at most [`WINDOW`] bytes at the top of the range, in
/// top-down slices.
fn populate_window(start: usize, len: usize) {
    let page = PAGE_SIZE.load(Ordering::Relaxed);
    // Unreachable once a populating policy is in force (`set_policy`
    // stores the page size first); checked rather than assumed because
    // this runs inside the allocator, where nothing may panic.
    if page == 0 {
        return;
    }
    for slice in window_slices(start, len, page, WINDOW) {
        populate(slice);
    }
}

/// The page-aligned slices, newest-used first, that pre-faulting
/// `[start, start + len)` populates: the whole pages within the top
/// `window` bytes of the range, from the top down in [`SLICE`]-sized
/// pieces (the lowest one shorter). `page` is a power of two.
fn window_slices(
    start: usize,
    len: usize,
    page: usize,
    window: usize,
) -> impl Iterator<Item = Range<usize>> {
    let top = (start + len) & !(page - 1);
    let bottom = (start + len.saturating_sub(window)).next_multiple_of(page);
    let mut next = top;
    std::iter::from_fn(move || {
        if next <= bottom {
            return None;
        }
        let end = next;
        next = end.saturating_sub(SLICE).max(bottom);
        Some(next..end)
    })
}

#[cfg(target_os = "linux")]
fn populate(pages: Range<usize>) {
    // SAFETY: MADV_POPULATE_WRITE faults the pages of the range in as a
    // write would, without changing their contents, and touches nothing
    // outside it. The range need not belong to the allocation any more
    // when the helper thread gets to it: populating memory that has been
    // reused is just as harmless, and a range that has been unmapped makes
    // the call fail with ENOMEM. The result is ignored on purpose: this is
    // an optimisation, and pages it does not populate fault in on first
    // touch as they would without it.
    unsafe {
        libc::madvise(
            pages.start as *mut libc::c_void,
            pages.end - pages.start,
            libc::MADV_POPULATE_WRITE,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn populate(_pages: Range<usize>) {}

#[cfg(target_os = "linux")]
mod background {
    use std::sync::{Mutex, OnceLock, PoisonError, TryLockError};
    use std::thread::Thread;

    /// How many ranges can wait. The helper takes the newest first, so by
    /// the time this many more large blocks have been allocated the oldest
    /// waiting one is long in use; a full queue drops it.
    const CAPACITY: usize = 64;

    /// The ranges waiting to be populated, in a fixed ring so that
    /// queueing from inside the allocator never allocates.
    pub(super) struct Pending {
        ranges: [(usize, usize); CAPACITY],
        /// Index of the oldest range.
        oldest: usize,
        len: usize,
    }

    impl Pending {
        pub(super) const fn new() -> Self {
            Pending {
                ranges: [(0, 0); CAPACITY],
                oldest: 0,
                len: 0,
            }
        }

        /// Adds a range, dropping the oldest when the ring is full.
        pub(super) fn push(&mut self, range: (usize, usize)) {
            if self.len == CAPACITY {
                self.oldest = (self.oldest + 1) % CAPACITY;
                self.len -= 1;
            }
            self.ranges[(self.oldest + self.len) % CAPACITY] = range;
            self.len += 1;
        }

        /// Takes the most recently added range: the chunk being filled now.
        pub(super) fn pop_newest(&mut self) -> Option<(usize, usize)> {
            if self.len == 0 {
                return None;
            }
            self.len -= 1;
            Some(self.ranges[(self.oldest + self.len) % CAPACITY])
        }
    }

    static QUEUE: Mutex<Pending> = Mutex::new(Pending::new());
    /// The helper thread, once [`start`] has spawned it.
    static WORKER: OnceLock<Thread> = OnceLock::new();
    /// Serialises [`start`], so that only one helper is ever spawned.
    static STARTING: Mutex<()> = Mutex::new(());

    /// Spawns the helper thread unless it is running already.
    pub(super) fn start() -> Result<(), String> {
        // The guarded data is `()`: a poisoned lock protects nothing.
        let _starting = STARTING.lock().unwrap_or_else(PoisonError::into_inner);
        if WORKER.get().is_some() {
            return Ok(());
        }
        let handle = std::thread::Builder::new()
            .name("brix-prefault".into())
            .spawn(worker)
            .map_err(|e| format!("cannot start the pre-faulting thread: {e}"))?;
        WORKER
            .set(handle.thread().clone())
            .expect("invariant: WORKER is only set here, under STARTING");
        Ok(())
    }

    /// Hands a range to the helper thread. Runs inside the allocator, so
    /// it never allocates, blocks or panics: without a helper, or while
    /// another thread holds the queue for its few instructions, the
    /// request is dropped and those pages fault in on first touch.
    pub(super) fn enqueue(start: usize, len: usize) {
        let Some(worker) = WORKER.get() else {
            return;
        };
        let mut queue = match QUEUE.try_lock() {
            Ok(queue) => queue,
            Err(TryLockError::WouldBlock) => return,
            // Nothing that runs under the lock can panic, so the queue is
            // consistent even if the lock were poisoned.
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        queue.push((start, len));
        drop(queue);
        worker.unpark();
    }

    fn worker() {
        loop {
            let next = QUEUE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_newest();
            match next {
                Some((start, len)) => super::populate_window(start, len),
                // `enqueue` unparks after queueing, so a range added
                // between the empty pop and this park wakes it at once.
                None => std::thread::park(),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod background {
    pub(super) fn start() -> Result<(), String> {
        Ok(())
    }

    pub(super) fn enqueue(_start: usize, _len: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    fn slices(start: usize, len: usize, window: usize) -> Vec<Range<usize>> {
        window_slices(start, len, PAGE, window).collect()
    }

    #[test]
    fn a_small_range_is_one_slice_of_whole_pages() {
        let start = 16 * PAGE + 100;
        let len = 3 * THRESHOLD + 17;
        assert_eq!(
            slices(start, len, WINDOW),
            vec![17 * PAGE..(start + len) / PAGE * PAGE]
        );
        // Less than a whole page: nothing to populate.
        assert_eq!(slices(PAGE + 1, PAGE, WINDOW), vec![]);
        assert_eq!(slices(PAGE, 0, WINDOW), vec![]);
    }

    #[test]
    fn slices_run_top_down_and_the_last_one_is_shorter() {
        let start = 1 << 30;
        let len = 2 * SLICE + 5 * PAGE;
        let top = start + len;
        assert_eq!(
            slices(start, len, WINDOW),
            vec![
                top - SLICE..top,
                top - 2 * SLICE..top - SLICE,
                start..top - 2 * SLICE,
            ]
        );
    }

    #[test]
    fn only_the_top_window_is_populated() {
        let start = 1 << 30;
        let len = WINDOW + 3 * SLICE + 123;
        let got = slices(start, len, WINDOW);
        let top = (start + len) / PAGE * PAGE;
        let bottom = (start + len - WINDOW).next_multiple_of(PAGE);
        assert_eq!(got.first(), Some(&(top - SLICE..top)));
        assert_eq!(got.last().map(|r| r.start), Some(bottom));
        // The window starts a page into the lowest slice.
        assert_eq!(got.len(), WINDOW / SLICE);
        assert_eq!(got.last().map(|r| r.len()), Some(SLICE - PAGE));
        assert!(got.windows(2).all(|w| w[1].end == w[0].start));
        assert_eq!(got.iter().map(|r| r.len()).sum::<usize>(), top - bottom);
        // A smaller window, as the residency test uses.
        assert_eq!(
            slices(start, 4 * SLICE, SLICE / 2),
            vec![start + 4 * SLICE - SLICE / 2..start + 4 * SLICE]
        );
    }

    #[test]
    fn fresh_ranges() {
        // A new block: all of it, from the threshold up.
        assert_eq!(fresh_range(0x1000, 0, THRESHOLD - 1), None);
        assert_eq!(fresh_range(0x1000, 0, THRESHOLD), Some((0x1000, THRESHOLD)));
        // A grown block: only the growth, once the block is large.
        assert_eq!(
            fresh_range(0x1000, THRESHOLD, 3 * THRESHOLD),
            Some((0x1000 + THRESHOLD, 2 * THRESHOLD))
        );
        assert_eq!(fresh_range(0x1000, 100, THRESHOLD / 2), None);
        // Shrinking or staying the same size writes nothing new.
        assert_eq!(fresh_range(0x1000, 3 * THRESHOLD, 2 * THRESHOLD), None);
        assert_eq!(fresh_range(0x1000, 2 * THRESHOLD, 2 * THRESHOLD), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_queue_hands_out_the_newest_range_and_drops_the_oldest_when_full() {
        let mut q = background::Pending::new();
        assert_eq!(q.pop_newest(), None);
        for i in 0..70 {
            q.push((i, 1));
        }
        // 64 fit; the 6 oldest were dropped.
        let mut got = Vec::new();
        while let Some((i, _)) = q.pop_newest() {
            got.push(i);
        }
        assert_eq!(got, (6..70).rev().collect::<Vec<_>>());
        q.push((1, 2));
        assert_eq!(q.pop_newest(), Some((1, 2)));
        assert_eq!(q.pop_newest(), None);
    }

    /// Populates the top of a fresh mapping and checks with `mincore`
    /// that exactly the window became resident.
    #[cfg(target_os = "linux")]
    #[test]
    fn populating_makes_the_top_resident_and_leaves_the_bottom_alone() {
        let page = populate_support()
            .expect("this test needs madvise(MADV_POPULATE_WRITE) (Linux 5.14 or later)");
        let len = 16 << 20;
        let window = 4 << 20;
        let mut mapping = memmap2::MmapOptions::new()
            .len(len)
            .no_reserve_swap()
            .map_anon()
            .expect("mmap failed");
        // A huge page could make memory just below the window resident.
        mapping
            .advise(memmap2::Advice::NoHugePage)
            .expect("MADV_NOHUGEPAGE failed");
        let base = mapping.as_mut_ptr().cast();
        let start = base as usize;
        for slice in window_slices(start, len, page, window) {
            populate(slice);
        }
        let mut resident = vec![0u8; len / page];
        // SAFETY: `resident` has one byte per page of the mapping.
        let r = unsafe { libc::mincore(base, len, resident.as_mut_ptr()) };
        assert_eq!(r, 0, "mincore failed");
        let pages = len / page;
        let window_pages = window / page;
        let is_resident = |i: usize| resident[i] & 1 == 1;
        assert!(
            (pages - window_pages..pages).all(is_resident),
            "the window is resident"
        );
        assert!(
            (0..pages - window_pages).all(|i| !is_resident(i)),
            "nothing below the window is resident"
        );
    }
}
