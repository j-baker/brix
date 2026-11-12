//! Compile-time storage selection. The serial crate aliases the original
//! standard-library types, preserving their layout and generated operations.
//! brix-eval-parallel compiles the same evaluator sources with brix_parallel.

#[cfg(not(brix_parallel))]
pub trait ThreadSafe {}
#[cfg(not(brix_parallel))]
impl<T: ?Sized> ThreadSafe for T {}
#[cfg(brix_parallel)]
pub trait ThreadSafe: Send + Sync {}
#[cfg(brix_parallel)]
impl<T: Send + Sync + ?Sized> ThreadSafe for T {}

#[cfg(not(brix_parallel))]
pub type Loader<T> = dyn Fn() -> Option<Vec<T>>;
#[cfg(brix_parallel)]
pub type Loader<T> = dyn Fn() -> Option<Vec<T>> + Send + Sync;

/// Per-thread evaluator state read on every call (the call depth and the
/// stack window). The serial build keeps a plain `Cell` in the evaluator. The
/// parallel build uses a native thread-local: looking a per-evaluator slot up
/// by thread id on every call cost several percent of evaluation. The state
/// describes the running thread (its stack, its nesting depth), and its users
/// restore it on the way out, so evaluators sharing a thread share it safely.
macro_rules! thread_cell {
    ($name:ident, $ty:ty, $init:expr) => {
        #[cfg(not(brix_parallel))]
        pub(crate) struct $name(std::cell::Cell<$ty>);
        #[cfg(not(brix_parallel))]
        impl $name {
            pub fn new() -> Self {
                Self(std::cell::Cell::new($init))
            }
            #[inline(always)]
            pub fn get(&self) -> $ty {
                self.0.get()
            }
            #[inline(always)]
            pub fn set(&self, value: $ty) {
                self.0.set(value)
            }
            #[inline(always)]
            #[allow(dead_code)]
            pub fn replace(&self, value: $ty) -> $ty {
                self.0.replace(value)
            }
        }
        #[cfg(brix_parallel)]
        pub(crate) struct $name;
        #[cfg(brix_parallel)]
        const _: () = {
            std::thread_local! {
                static VALUE: std::cell::Cell<$ty> = const { std::cell::Cell::new($init) };
            }
            impl $name {
                pub fn new() -> Self {
                    Self
                }
                #[inline(always)]
                pub fn get(&self) -> $ty {
                    VALUE.get()
                }
                #[inline(always)]
                pub fn set(&self, value: $ty) {
                    VALUE.set(value)
                }
                #[inline(always)]
                #[allow(dead_code)]
                pub fn replace(&self, value: $ty) -> $ty {
                    VALUE.replace(value)
                }
            }
        };
    };
}
pub(crate) use thread_cell;

#[cfg(brix_parallel)]
std::thread_local! {
    static BACKGROUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark this thread as evaluating a background spark (`parallel::sparks`).
#[cfg(brix_parallel)]
pub(crate) fn set_background(background: bool) {
    BACKGROUND.set(background);
}

/// Called before output the user sees in evaluation order (a warning, say)
/// by code without a `Machine` at hand. A background spark stops here and
/// leaves the operation to the ordered consumer; see `Machine::ordered_effect`.
#[inline]
#[track_caller]
pub(crate) fn ordered_effect() -> crate::Result<()> {
    #[cfg(brix_parallel)]
    if BACKGROUND.get() {
        note_deferral(std::panic::Location::caller());
        return Err(crate::error::deferred());
    }
    Ok(())
}

/// Why background sparks stopped, by reason, for `BRIX_TIMINGS`: a guard that
/// stops most of them is parallelism lost.
#[cfg(brix_parallel)]
static DEFERRALS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(brix_parallel)]
#[cold]
pub(crate) fn note_deferral(reason: impl std::fmt::Display) {
    let mut deferrals = DEFERRALS.lock().unwrap_or_else(|p| p.into_inner());
    *deferrals.entry(reason.to_string()).or_default() += 1;
}

/// The most frequent reasons, most frequent first.
#[cfg(brix_parallel)]
pub(crate) fn deferrals() -> String {
    let deferrals = DEFERRALS.lock().unwrap_or_else(|p| p.into_inner());
    let mut reasons: Vec<_> = deferrals.iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(a.1));
    reasons
        .iter()
        .take(12)
        .map(|(reason, count)| format!("\n  spark stopped {count} times: {reason}"))
        .collect()
}

#[cfg(not(brix_parallel))]
#[derive(Default)]
pub(crate) struct Gate;

#[cfg(not(brix_parallel))]
pub(crate) struct GateGuard;
#[cfg(not(brix_parallel))]
impl Gate {
    #[inline(always)]
    pub fn lock(&self) -> GateGuard {
        GateGuard
    }
}
#[cfg(brix_parallel)]
pub(crate) type Gate = parking_lot::ReentrantMutex<()>;

#[cfg(not(brix_parallel))]
pub use bumpalo::Bump;
#[cfg(not(brix_parallel))]
pub(crate) use std::{cell, rc};

#[cfg(brix_parallel)]
pub mod cell {
    use crossbeam_utils::atomic::AtomicCell;
    use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    #[derive(Debug)]
    pub struct Cell<T: Copy>(AtomicCell<T>);

    impl<T: Copy> Cell<T> {
        pub const fn new(value: T) -> Self {
            Self(AtomicCell::new(value))
        }
        #[inline]
        pub fn get(&self) -> T {
            self.0.load()
        }
        #[inline]
        pub fn set(&self, value: T) {
            self.0.store(value)
        }
        pub fn replace(&self, value: T) -> T {
            self.0.swap(value)
        }
    }

    impl<T: Copy + Default> Default for Cell<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T: Copy + Eq> Cell<T> {
        pub fn update(&self, mut f: impl FnMut(T) -> T) {
            let mut previous = self.get();
            loop {
                match self.0.compare_exchange(previous, f(previous)) {
                    Ok(_) => return,
                    Err(current) => previous = current,
                }
            }
        }
    }

    impl Cell<u64> {
        pub fn fetch_add(&self, value: u64) -> u64 {
            self.0.fetch_add(value)
        }
    }

    pub type Ref<'a, T> = RwLockReadGuard<'a, T>;

    #[derive(Debug, Default)]
    pub struct RefCell<T>(RwLock<T>);

    impl<T> RefCell<T> {
        pub const fn new(value: T) -> Self {
            Self(RwLock::new(value))
        }
        pub fn borrow(&self) -> Ref<'_, T> {
            self.0.read_recursive()
        }
        pub fn borrow_mut(&self) -> RwLockWriteGuard<'_, T> {
            self.0.write()
        }
    }

    pub type OnceCell<T> = std::sync::OnceLock<T>;
}

#[cfg(brix_parallel)]
pub(crate) mod rc {
    pub use std::sync::Arc as Rc;
}

#[cfg(brix_parallel)]
pub use arena::Bump;

#[cfg(brix_parallel)]
mod arena {
    #![allow(unsafe_code)]
    // Like bumpalo, each method reserves fresh disjoint storage.
    #![allow(clippy::mut_from_ref)]

    use parking_lot::Mutex;
    use std::alloc::Layout;
    use std::ptr::NonNull;

    /// Permanent metadata arena. Allocation reserves disjoint memory while
    /// locked; initialization occurs after unlocking, so fill callbacks may
    /// allocate recursively. Like bumpalo, dropping the arena frees storage
    /// without running object destructors. Existing references exclude reset.
    #[derive(Default)]
    pub struct Bump(Mutex<bumpalo::Bump>);

    impl Bump {
        pub fn with_capacity(bytes: usize) -> Self {
            Self(Mutex::new(bumpalo::Bump::with_capacity(bytes)))
        }
        pub fn new() -> Self {
            Self::default()
        }

        pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
            self.0.lock().alloc_layout(layout)
        }

        pub fn alloc<T>(&self, value: T) -> &mut T {
            let pointer = self.alloc_layout(Layout::new::<T>()).cast::<T>();
            // SAFETY: this fresh allocation is disjoint from every earlier
            // allocation and remains alive until the arena is dropped.
            unsafe {
                pointer.as_ptr().write(value);
                &mut *pointer.as_ptr()
            }
        }

        pub fn alloc_slice_fill_with<T>(
            &self,
            len: usize,
            mut fill: impl FnMut(usize) -> T,
        ) -> &mut [T] {
            let pointer = self
                .alloc_layout(Layout::array::<T>(len).expect("metadata array fits"))
                .cast::<T>();
            // SAFETY: the reservation holds len disjoint, aligned elements.
            // No partially initialized slice escapes, including on unwind.
            unsafe {
                for index in 0..len {
                    pointer.as_ptr().add(index).write(fill(index));
                }
                std::slice::from_raw_parts_mut(pointer.as_ptr(), len)
            }
        }

        pub fn alloc_slice_copy<T: Copy>(&self, values: &[T]) -> &mut [T] {
            self.alloc_slice_fill_with(values.len(), |index| values[index])
        }

        pub fn alloc_slice_fill_copy<T: Copy>(&self, len: usize, value: T) -> &mut [T] {
            self.alloc_slice_fill_with(len, |_| value)
        }

        pub fn alloc_slice_fill_iter<T, I: IntoIterator<Item = T>>(&self, values: I) -> &mut [T]
        where
            I::IntoIter: ExactSizeIterator,
        {
            let mut values = values.into_iter();
            self.alloc_slice_fill_with(values.len(), |_| {
                values.next().expect("exact-size metadata iterator")
            })
        }

        pub fn alloc_str(&self, text: &str) -> &mut str {
            // SAFETY: copying preserves the input's UTF-8 validity.
            unsafe { std::str::from_utf8_unchecked_mut(self.alloc_slice_copy(text.as_bytes())) }
        }

        pub fn allocated_bytes(&self) -> usize {
            self.0.lock().allocated_bytes()
        }

        /// # Safety
        /// As for bumpalo's raw chunk iterator, payload inspection must not
        /// race allocation/initialization. The host uses only chunk lengths,
        /// after all evaluator workers have joined.
        pub unsafe fn iter_allocated_chunks_raw(&self) -> std::vec::IntoIter<(*mut u8, usize)> {
            unsafe {
                self.0
                    .lock()
                    .iter_allocated_chunks_raw()
                    .collect::<Vec<_>>()
                    .into_iter()
            }
        }
    }
}

/// Read-mostly permanent metadata. Fat pointers (slices and closure trait
/// objects) otherwise require AtomicCell's fallback lock. Publish an immutable
/// arena snapshot through one native atomic pointer instead. Old snapshots are
/// reclaimed with the metadata arena, so readers need no lock or refcount.
#[cfg(not(brix_parallel))]
#[repr(transparent)]
pub(crate) struct Published<'a, T: Copy + Default> {
    value: std::cell::Cell<T>,
    arena: std::marker::PhantomData<&'a Bump>,
}
#[cfg(not(brix_parallel))]
impl<'a, T: Copy + Default> Published<'a, T> {
    pub fn new(value: T, _: &'a Bump) -> Self {
        Self {
            value: std::cell::Cell::new(value),
            arena: std::marker::PhantomData,
        }
    }
    pub fn empty(arena: &'a Bump) -> Self {
        Self::new(T::default(), arena)
    }
    #[inline]
    pub fn get(&self) -> T {
        self.value.get()
    }
    #[inline]
    pub fn set(&self, value: T) {
        self.value.set(value);
    }
}
#[cfg(brix_parallel)]
pub(crate) struct Published<'a, T: Copy + Default> {
    value: std::sync::atomic::AtomicPtr<T>,
    arena: &'a Bump,
    payload: std::marker::PhantomData<T>,
}
#[cfg(brix_parallel)]
impl<'a, T: Copy + Default> Published<'a, T> {
    pub fn empty(arena: &'a Bump) -> Self {
        Self {
            value: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            arena,
            payload: std::marker::PhantomData,
        }
    }
    pub fn new(value: T, arena: &'a Bump) -> Self {
        Self {
            value: std::sync::atomic::AtomicPtr::new(arena.alloc(value)),
            arena,
            payload: std::marker::PhantomData,
        }
    }
    #[inline]
    #[allow(unsafe_code)]
    pub fn get(&self) -> T {
        let pointer = self.value.load(std::sync::atomic::Ordering::Acquire);
        if pointer.is_null() {
            T::default()
        } else {
            // SAFETY: publication initializes a disjoint immutable snapshot;
            // the arena outlives this cell and every access through it.
            unsafe { *pointer }
        }
    }
    pub fn set(&self, value: T) {
        self.value.store(
            self.arena.alloc(value),
            std::sync::atomic::Ordering::Release,
        );
    }
}
